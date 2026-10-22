use std::collections::HashMap;
use std::fs::{self, File};
use std::path::Path;

use memmap2::Mmap;

use crate::char_property::CharProperty;
use crate::error::Error;
use crate::matrix::Matrix;
use crate::system_dictionary::{DictionaryKind, PrefixHit, SystemDictionary, TokenRecord};

const OPTION_DEFAULT_MAX_GROUPING_SIZE: usize = 24;
const BOS_FEATURE_KEY: &[u8] = b"bos-feature";
const UNK_FEATURE_KEY: &[u8] = b"unk-feature";

pub(crate) fn map_file(path: &Path) -> Result<Mmap, Error> {
    let io_error = |source| Error::Io { path: path.to_path_buf(), source };
    let file = File::open(path).map_err(io_error)?;
    unsafe { Mmap::map(&file) }.map_err(io_error)
}

const _: () = assert!(cfg!(target_endian = "little"));

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn dicrc_values(bytes: &[u8], path: &Path) -> Result<HashMap<Vec<u8>, Vec<u8>>, Error> {
    let mut values = HashMap::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        if line.is_empty() || line[0] == b';' || line[0] == b'#' {
            continue;
        }
        let Some(equals) = line.iter().position(|&byte| byte == b'=') else {
            return Err(Error::Format {
                file: path.to_path_buf(),
                reason: "a line has no '='",
            });
        };
        let key_end = line[..equals]
            .iter()
            .rposition(|&byte| !is_space(byte))
            .map_or(0, |at| at + 1);
        let value_start = line[equals + 1..]
            .iter()
            .position(|&byte| !is_space(byte))
            .map_or(line.len(), |at| equals + 1 + at);
        values
            .entry(line[..key_end].to_vec())
            .or_insert_with(|| line[value_start..].to_vec());
    }
    Ok(values)
}

pub struct Dictionary {
    pub(crate) system: SystemDictionary,
    pub(crate) unknown: SystemDictionary,
    pub(crate) matrix: Matrix,
    pub(crate) chars: CharProperty,
    pub(crate) unknown_tokens: Vec<Box<[TokenRecord]>>,
    pub(crate) bos_feature: Vec<u8>,
    pub(crate) max_grouping_size: usize,
}

impl Dictionary {
    pub fn max_grouping_size(&self) -> usize {
        self.max_grouping_size
    }

    pub fn set_max_grouping_size(&mut self, size: usize) {
        self.max_grouping_size = size;
    }

    pub fn open(directory: impl AsRef<Path>) -> Result<Self, Error> {
        let directory = directory.as_ref();
        let dicrc = directory.join("dicrc");
        let bytes = fs::read(&dicrc)
            .map_err(|source| Error::Io { path: dicrc.clone(), source })?;
        let values = dicrc_values(&bytes, &dicrc)?;
        if values.get(UNK_FEATURE_KEY).is_some_and(|value| !value.is_empty()) {
            return Err(Error::Unsupported("the unk-feature setting in dicrc"));
        }
        let bos_feature = values
            .get(BOS_FEATURE_KEY)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| Error::Format { file: dicrc.clone(), reason: "bos-feature is missing" })?;
        let system = SystemDictionary::open(&directory.join("sys.dic"), DictionaryKind::System)?;
        let unknown_path = directory.join("unk.dic");
        let unknown = SystemDictionary::open(&unknown_path, DictionaryKind::Unknown)?;
        let matrix_path = directory.join("matrix.bin");
        let matrix = Matrix::open(&matrix_path)?;
        let chars = CharProperty::open(&directory.join("char.bin"))?;
        let matches_matrix = |dictionary: &SystemDictionary| {
            dictionary.left_size() == matrix.left_size() && dictionary.right_size() == matrix.right_size()
        };
        if !matches_matrix(&system) || !matches_matrix(&unknown) {
            return Err(Error::Format {
                file: matrix_path,
                reason: "the matrix size does not match sys.dic or unk.dic",
            });
        }
        let broken_unknown = |reason| Error::Format { file: unknown_path.clone(), reason };
        let mut unknown_tokens = Vec::with_capacity(chars.category_count());
        for category in 0..chars.category_count() {
            let value = unknown
                .exact_match(chars.name(category))
                .ok_or_else(|| broken_unknown("a character category of char.bin is missing"))?;
            let hit = PrefixHit { value, length: 0 };
            let tokens = (hit.first_token()..hit.first_token() + hit.token_count())
                .map(|index| unknown.token(index))
                .collect::<Result<Box<[_]>, _>>()
                .map_err(|_| broken_unknown("an unknown-word token is not valid"))?;
            unknown_tokens.push(tokens);
        }
        Ok(Self {
            system,
            unknown,
            matrix,
            chars,
            unknown_tokens,
            bos_feature,
            max_grouping_size: OPTION_DEFAULT_MAX_GROUPING_SIZE,
        })
    }
}
