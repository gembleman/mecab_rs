use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use crate::char_property::CharProperty;
use crate::error::Error;
use crate::expand_cache::{self, Expansion};
use crate::matrix::Matrix;
use crate::packed::PackedDictionary;
use crate::system_dictionary::{DictionaryKind, SystemDictionary, TokenRecord};

const MAX_USER_DICTIONARIES: usize = u8::MAX as usize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserIndex(u8);

impl UserIndex {
    pub(crate) fn from_position(position: usize) -> Self {
        debug_assert!(position < MAX_USER_DICTIONARIES);
        Self(position as u8)
    }

    pub(crate) fn position(self) -> usize {
        usize::from(self.0)
    }

    pub(crate) fn number(self) -> u64 {
        u64::from(self.0) + 1
    }
}
const DEFAULT_MAX_GROUPING_SIZE: NonZeroUsize = NonZeroUsize::new(24).unwrap();

const _: () = assert!(cfg!(target_endian = "little"));
const _: () = assert!(usize::BITS >= 64);

pub struct Dictionary {
    pub(crate) system: Arc<PackedDictionary>,
    pub(crate) users: Vec<SystemDictionary>,
    pub(crate) unknown: Arc<PackedDictionary>,
    pub(crate) matrix: Matrix,
    pub(crate) chars: CharProperty,
    pub(crate) unknown_tokens: Vec<Box<[TokenRecord]>>,
    pub(crate) bos_feature: String,
    pub(crate) unknown_feature: Option<String>,
    pub(crate) max_grouping_size: NonZeroUsize,
}

impl Dictionary {
    pub fn max_grouping_size(&self) -> NonZeroUsize {
        self.max_grouping_size
    }

    pub fn set_max_grouping_size(&mut self, size: NonZeroUsize) {
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
        let cache = expand_cache::ensure(path)?;
        match PackedDictionary::open(&cache.path) {
            Ok(packed) => Self::open_packed(&cache.path, packed, user_dictionaries),
            Err(Error::Format { .. }) if cache.expansion == Expansion::Reused => {
                expand_cache::discard_damaged(&cache.path)?;
                let cache = expand_cache::ensure(path)?;
                let packed = PackedDictionary::open(&cache.path)?;
                Self::open_packed(&cache.path, packed, user_dictionaries)
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
        let matrix = Self::open_matrix(path, &packed)?;
        let chars = CharProperty::open_bytes(path, packed.char_table_mapped())?;
        let users = Self::open_users(user_dictionaries)?;
        let sizes = (matrix.left_size(), matrix.right_size());
        let user_sizes = users
            .iter()
            .map(|user| (user.left_size(), user.right_size()));
        let dictionary_sizes = std::iter::once((packed.left_size(), packed.right_size()));
        if !dictionary_sizes
            .chain(user_sizes)
            .all(|found| found == sizes)
        {
            return Err(Error::format(
                path,
                "the matrix size does not match the packed dictionary or a user dictionary",
            ));
        }
        let unknown_tokens = Self::collect_unknown_tokens(&packed);
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

    fn open_matrix(path: &Path, packed: &PackedDictionary) -> Result<Matrix, Error> {
        match packed.templates_mapped() {
            Some(templates) => Matrix::open_hot_bytes(path, packed.matrix_mapped(), templates),
            None => Matrix::open_dense_bytes(path, packed.matrix_mapped()),
        }
    }

    fn open_users<P: AsRef<Path>>(user_dictionaries: &[P]) -> Result<Vec<SystemDictionary>, Error> {
        user_dictionaries
            .iter()
            .map(|user| SystemDictionary::open(user.as_ref(), DictionaryKind::User))
            .collect()
    }

    fn collect_unknown_tokens(packed: &PackedDictionary) -> Vec<Box<[TokenRecord]>> {
        (0..packed.category_count())
            .map(|category| {
                packed
                    .unknown_tokens(category)
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            })
            .collect()
    }
}
