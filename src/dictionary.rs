use std::collections::HashMap;
use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

use crate::build::text::key_value;
use crate::char_property::CharProperty;
use crate::char_table::CHAR_TABLE_FILE;
use crate::error::Error;
use crate::matrix::Matrix;
use crate::packed::PackedDictionary;
use crate::system_dictionary::{DictionaryKind, PrefixHit, SystemDictionary, TokenRecord};

const DENSE_MATRIX_FILE: &str = "matrix.bin";
const HOT_MATRIX_FILE: &str = "matrix.hot";
const TEMPLATES_FILE: &str = "matrix.tpl";
const INT_TEMPLATES_FILE: &str = "matrix.itp";

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
        let Some((key, value)) = key_value(line) else {
            return Err(Error::Format {
                file: path.to_path_buf(),
                reason: "a line has no '='",
            });
        };
        values.entry(key.to_vec()).or_insert_with(|| value.to_vec());
    }
    Ok(values)
}

pub struct Dictionary {
    pub(crate) system: Lexicon,
    pub(crate) users: Vec<SystemDictionary>,
    pub(crate) unknown: Lexicon,
    pub(crate) matrix: Matrix,
    pub(crate) chars: CharProperty,
    pub(crate) unknown_tokens: Vec<Box<[TokenRecord]>>,
    pub(crate) bos_feature: String,
    pub(crate) unknown_feature: Option<String>,
    pub(crate) max_grouping_size: usize,
}

pub(crate) enum Lexicon {
    System(SystemDictionary),
    Packed(Arc<PackedDictionary>),
}

impl Lexicon {
    #[inline(always)]
    pub(crate) fn exact_match(&self, key: &[u8]) -> Option<std::ops::Range<usize>> {
        match self {
            Self::System(dictionary) => dictionary.exact_match(key),
            Self::Packed(dictionary) => dictionary.exact_match(key),
        }
    }

    #[inline(always)]
    pub(crate) fn token(&self, index: usize) -> TokenRecord {
        match self {
            Self::System(dictionary) => dictionary.token(index),
            Self::Packed(dictionary) => dictionary.token(index),
        }
    }

    #[inline(always)]
    pub(crate) fn feature(&self, offset: u32) -> &str {
        match self {
            Self::System(dictionary) => dictionary.feature(offset),
            Self::Packed(dictionary) => dictionary.feature(offset),
        }
    }

    #[inline(always)]
    pub(crate) fn left_size(&self) -> usize {
        match self {
            Self::System(dictionary) => dictionary.left_size(),
            Self::Packed(dictionary) => dictionary.left_size(),
        }
    }

    #[inline(always)]
    pub(crate) fn right_size(&self) -> usize {
        match self {
            Self::System(dictionary) => dictionary.right_size(),
            Self::Packed(dictionary) => dictionary.right_size(),
        }
    }
}

pub(crate) trait LexiconOps {
    fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<PrefixHit>);
    fn token(&self, index: usize) -> TokenRecord;
    fn feature(&self, offset: u32) -> &str;
}

impl LexiconOps for PackedDictionary {
    fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<PrefixHit>) {
        self.common_prefix_search(key, hits)
    }

    fn token(&self, index: usize) -> TokenRecord {
        self.token(index)
    }

    fn feature(&self, offset: u32) -> &str {
        self.feature(offset)
    }
}

impl LexiconOps for SystemDictionary {
    fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<PrefixHit>) {
        self.common_prefix_search(key, hits)
    }

    fn token(&self, index: usize) -> TokenRecord {
        self.token(index)
    }

    fn feature(&self, offset: u32) -> &str {
        self.feature(offset)
    }
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
        if user_dictionaries.len() > MAX_USER_DICTIONARIES {
            return Err(Error::Unsupported("more than 255 user dictionaries"));
        }
        if directory.is_file() {
            return Self::open_packed(directory, user_dictionaries);
        }
        let users: Vec<&Path> = user_dictionaries.iter().map(AsRef::as_ref).collect();
        let hot_path = directory.join(HOT_MATRIX_FILE);
        let int_templates_path = directory.join(INT_TEMPLATES_FILE);
        let templates_path = directory.join(TEMPLATES_FILE);
        if hot_path.exists() && int_templates_path.exists() {
            Self::open_matrix(directory, &users, HOT_MATRIX_FILE, |path| {
                Matrix::open_hot_int(path, &int_templates_path)
            })
        } else if hot_path.exists() && templates_path.exists() {
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
        let not_utf8 = |_| Error::Format {
            file: dicrc.clone(),
            reason: "bos-feature or unk-feature is not valid UTF-8",
        };
        let unknown_feature = values
            .get(UNK_FEATURE_KEY)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8(value.clone()).map_err(not_utf8))
            .transpose()?;
        let bos_feature = values
            .get(BOS_FEATURE_KEY)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Format {
                file: dicrc.clone(),
                reason: "bos-feature is missing",
            })?;
        let bos_feature = String::from_utf8(bos_feature.clone()).map_err(not_utf8)?;
        let system = Lexicon::System(SystemDictionary::open(
            &directory.join("sys.dic"),
            DictionaryKind::System,
        )?);
        let users = user_paths
            .iter()
            .map(|path| SystemDictionary::open(path, DictionaryKind::User))
            .collect::<Result<Vec<_>, _>>()?;
        let unknown_path = directory.join("unk.dic");
        let unknown = Lexicon::System(SystemDictionary::open(
            &unknown_path,
            DictionaryKind::Unknown,
        )?);
        let matrix_path = directory.join(matrix_file);
        let matrix = open_matrix(&matrix_path)?;
        let chars = CharProperty::open(&directory.join(CHAR_TABLE_FILE))?;
        let matches_matrix = |dictionary: &Lexicon| {
            dictionary.left_size() == matrix.left_size()
                && dictionary.right_size() == matrix.right_size()
        };
        if !matches_matrix(&system)
            || !matches_matrix(&unknown)
            || !users.iter().all(|user| {
                user.left_size() == matrix.left_size() && user.right_size() == matrix.right_size()
            })
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

    fn open_packed<P: AsRef<Path>>(path: &Path, user_dictionaries: &[P]) -> Result<Self, Error> {
        let packed = Arc::new(PackedDictionary::open(path)?);
        let matrix = if packed.template_bytes().is_some() {
            if packed.integer_templates() {
                Matrix::open_hot_int_bytes(
                    path,
                    packed.matrix_mapped(),
                    packed.templates_mapped().unwrap(),
                )?
            } else {
                Matrix::open_hot_bytes(
                    path,
                    packed.matrix_mapped(),
                    packed.templates_mapped().unwrap(),
                )?
            }
        } else {
            Matrix::open_dense_bytes(path, packed.matrix_mapped())?
        };
        let chars = CharProperty::open_bytes(path, packed.char_table_mapped())?;
        let unknown = Lexicon::Packed(Arc::clone(&packed));
        let users = user_dictionaries
            .iter()
            .map(|user| SystemDictionary::open(user.as_ref(), DictionaryKind::User))
            .collect::<Result<Vec<_>, _>>()?;
        let matches_matrix = |dictionary: &Lexicon| {
            dictionary.left_size() == matrix.left_size()
                && dictionary.right_size() == matrix.right_size()
        };
        if !matches_matrix(&unknown)
            || !users.iter().all(|user| {
                user.left_size() == matrix.left_size() && user.right_size() == matrix.right_size()
            })
        {
            return Err(Error::Format {
                file: path.to_path_buf(),
                reason: "the matrix size does not match the packed dictionary or a user dictionary",
            });
        }
        let packed_ref = match &unknown {
            Lexicon::Packed(dictionary) => dictionary,
            Lexicon::System(_) => unreachable!(),
        };
        let unknown_tokens = (0..packed_ref.category_count())
            .map(|category| {
                packed_ref
                    .unknown_tokens(category)
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            })
            .collect();
        let bos_feature = packed_ref.bos_feature().to_owned();
        let unknown_feature = packed_ref.unknown_feature().map(str::to_owned);
        Ok(Self {
            system: Lexicon::Packed(packed),
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
