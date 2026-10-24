use std::collections::HashMap;
use std::fs::{self, File};
use std::path::Path;

use memmap2::Mmap;

use crate::char_property::CharProperty;
use crate::char_table::CHAR_TABLE_FILE;
use crate::error::Error;
use crate::matrix::Matrix;
use crate::system_dictionary::{DictionaryKind, SystemDictionary, TokenRecord};

const DENSE_MATRIX_FILE: &str = "matrix.bin";
const HOT_MATRIX_FILE: &str = "matrix.hot";
const TEMPLATES_FILE: &str = "matrix.tpl";

const MAX_USER_DICTIONARIES: usize = u8::MAX as usize;
const DEFAULT_MAX_GROUPING_SIZE: usize = 24;
const BOS_FEATURE_KEY: &[u8] = b"bos-feature";
const UNK_FEATURE_KEY: &[u8] = b"unk-feature";

pub(crate) fn map_file(path: &Path) -> Result<Mmap, Error> {
    let io_error = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(io_error)?;
    unsafe { Mmap::map(&file) }.map_err(io_error)
}

const _: () = assert!(cfg!(target_endian = "little"));

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
            .rposition(|byte| !byte.is_ascii_whitespace())
            .map_or(0, |at| at + 1);
        let value_start = line[equals + 1..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map_or(line.len(), |at| equals + 1 + at);
        values
            .entry(line[..key_end].to_vec())
            .or_insert_with(|| line[value_start..].to_vec());
    }
    Ok(values)
}

pub struct Dictionary {
    pub(crate) system: SystemDictionary,
    pub(crate) users: Vec<SystemDictionary>,
    pub(crate) unknown: SystemDictionary,
    pub(crate) matrix: Matrix,
    pub(crate) chars: CharProperty,
    pub(crate) unknown_tokens: Vec<Box<[TokenRecord]>>,
    pub(crate) bos_feature: Vec<u8>,
    pub(crate) unknown_feature: Option<Vec<u8>>,
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
        Self::open_with_user(directory, &[] as &[&Path])
    }

    pub fn open_with_user<P: AsRef<Path>>(
        directory: impl AsRef<Path>,
        user_dictionaries: &[P],
    ) -> Result<Self, Error> {
        let directory = directory.as_ref();
        let users: Vec<&Path> = user_dictionaries.iter().map(AsRef::as_ref).collect();
        if users.len() > MAX_USER_DICTIONARIES {
            return Err(Error::Unsupported("more than 255 user dictionaries"));
        }
        let hot_path = directory.join(HOT_MATRIX_FILE);
        let templates_path = directory.join(TEMPLATES_FILE);
        if hot_path.exists() && templates_path.exists() {
            Self::open_matrix(directory, &users, HOT_MATRIX_FILE, |path| {
                Matrix::open_hot(path, &templates_path)
            })
        } else {
            Self::open_matrix(directory, &users, DENSE_MATRIX_FILE, Matrix::open_dense)
        }
    }

    fn open_matrix(
        directory: &Path,
        user_paths: &[&Path],
        matrix_file: &str,
        open_matrix: impl FnOnce(&Path) -> Result<Matrix, Error>,
    ) -> Result<Self, Error> {
        let dicrc = directory.join("dicrc");
        let bytes = fs::read(&dicrc).map_err(|source| Error::Io {
            path: dicrc.clone(),
            source,
        })?;
        let values = dicrc_values(&bytes, &dicrc)?;
        let unknown_feature = values
            .get(UNK_FEATURE_KEY)
            .filter(|value| !value.is_empty())
            .cloned();
        let bos_feature = values
            .get(BOS_FEATURE_KEY)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| Error::Format {
                file: dicrc.clone(),
                reason: "bos-feature is missing",
            })?;
        let system = SystemDictionary::open(&directory.join("sys.dic"), DictionaryKind::System)?;
        let users = user_paths
            .iter()
            .map(|path| SystemDictionary::open(path, DictionaryKind::User))
            .collect::<Result<Vec<_>, _>>()?;
        let unknown_path = directory.join("unk.dic");
        let unknown = SystemDictionary::open(&unknown_path, DictionaryKind::Unknown)?;
        let matrix_path = directory.join(matrix_file);
        let matrix = open_matrix(&matrix_path)?;
        let chars = CharProperty::open(&directory.join(CHAR_TABLE_FILE))?;
        let matches_matrix = |dictionary: &SystemDictionary| {
            dictionary.left_size() == matrix.left_size()
                && dictionary.right_size() == matrix.right_size()
        };
        if !matches_matrix(&system)
            || !matches_matrix(&unknown)
            || !users.iter().all(matches_matrix)
        {
            return Err(Error::Format {
                file: matrix_path,
                reason: "the matrix size does not match sys.dic, unk.dic or a user dictionary",
            });
        }
        let broken_unknown = |reason| Error::Format {
            file: unknown_path.clone(),
            reason,
        };
        let mut unknown_tokens = Vec::with_capacity(chars.category_count());
        for category in 0..chars.category_count() {
            let run = unknown
                .exact_match(chars.name(category))
                .ok_or_else(|| broken_unknown("a character category of char.uni is missing"))?;
            if !unknown.holds_tokens(&run) {
                return Err(broken_unknown("an unknown-word token is not valid"));
            }
            let tokens = run.map(|index| unknown.token(index)).collect();
            unknown_tokens.push(tokens);
        }
        Ok(Self {
            system,
            users,
            unknown,
            matrix,
            chars,
            unknown_tokens,
            bos_feature,
            unknown_feature,
            max_grouping_size: DEFAULT_MAX_GROUPING_SIZE,
        })
    }
}
