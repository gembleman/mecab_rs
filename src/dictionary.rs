use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

use crate::char_property::CharProperty;
use crate::error::Error;
use crate::expand_cache;
use crate::matrix::Matrix;
use crate::packed::PackedDictionary;
use crate::system_dictionary::{DictionaryKind, SystemDictionary, TokenRecord};

const MAX_USER_DICTIONARIES: usize = u8::MAX as usize;
const DEFAULT_MAX_GROUPING_SIZE: usize = 24;

pub(crate) fn map_file(path: &Path) -> Result<Mmap, Error> {
    let io_error = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(io_error)?;
    unsafe { Mmap::map(&file) }.map_err(io_error)
}

const _: () = assert!(cfg!(target_endian = "little"));

pub struct Dictionary {
    pub(crate) system: Arc<PackedDictionary>,
    pub(crate) users: Vec<SystemDictionary>,
    pub(crate) unknown: Arc<PackedDictionary>,
    pub(crate) matrix: Matrix,
    pub(crate) chars: CharProperty,
    pub(crate) unknown_tokens: Vec<Box<[TokenRecord]>>,
    pub(crate) bos_feature: String,
    pub(crate) unknown_feature: Option<String>,
    pub(crate) max_grouping_size: usize,
}

impl Dictionary {
    pub fn max_grouping_size(&self) -> usize {
        self.max_grouping_size
    }

    pub fn set_max_grouping_size(&mut self, size: usize) {
        self.max_grouping_size = size;
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_user(path, &[] as &[&Path])
    }

    pub fn open_with_user<P: AsRef<Path>>(
        path: impl AsRef<Path>,
        user_dictionaries: &[P],
    ) -> Result<Self, Error> {
        if user_dictionaries.len() > MAX_USER_DICTIONARIES {
            return Err(Error::Unsupported("more than 255 user dictionaries"));
        }
        let path = path.as_ref();
        if path.is_dir() {
            return Err(Error::Unsupported(
                "dictionary directories are not accepted; convert with `mecab-rs-migrate pack <dir> <out.mrd>`",
            ));
        }
        if !expand_cache::is_xz(path) {
            return Self::open_packed(path, PackedDictionary::open(path)?, user_dictionaries);
        }
        let (cache, expanded) = expand_cache::ensure(path, false)?;
        match PackedDictionary::open(&cache) {
            Ok(packed) => Self::open_packed(&cache, packed, user_dictionaries),
            Err(_) if !expanded => {
                let (cache, _) = expand_cache::ensure(path, true)?;
                Self::open_packed(&cache, PackedDictionary::open(&cache)?, user_dictionaries)
            }
            Err(error) => Err(error),
        }
    }

    fn open_packed<P: AsRef<Path>>(
        path: &Path,
        packed: PackedDictionary,
        user_dictionaries: &[P],
    ) -> Result<Self, Error> {
        let packed = Arc::new(packed);
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
        let users = user_dictionaries
            .iter()
            .map(|user| SystemDictionary::open(user.as_ref(), DictionaryKind::User))
            .collect::<Result<Vec<_>, _>>()?;
        let matches_matrix = |dictionary: &PackedDictionary| {
            dictionary.left_size() == matrix.left_size()
                && dictionary.right_size() == matrix.right_size()
        };
        if !matches_matrix(&packed)
            || !users.iter().all(|user| {
                user.left_size() == matrix.left_size() && user.right_size() == matrix.right_size()
            })
        {
            return Err(Error::Format {
                file: path.to_path_buf(),
                reason: "the matrix size does not match the packed dictionary or a user dictionary",
            });
        }
        let unknown_tokens = (0..packed.category_count())
            .map(|category| {
                packed
                    .unknown_tokens(category)
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            })
            .collect();
        let bos_feature = packed.bos_feature().to_owned();
        let unknown_feature = packed.unknown_feature().map(str::to_owned);
        Ok(Self {
            system: Arc::clone(&packed),
            users,
            unknown: packed,
            matrix,
            chars,
            unknown_tokens,
            bos_feature,
            unknown_feature,
            max_grouping_size: DEFAULT_MAX_GROUPING_SIZE,
        })
    }
}
