use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use self::source::read_at;
use self::token_format::TokenFormat;
use super::features::{FeatureView, NumSpec};
use super::{
    BLOCK_CODES, CELLS, CHAR_BLOCKS, CHAR_INDEX, CHAR_TABLE, DICRC, FEATURES, HEADER_BYTES, MATRIX,
    SECTIONS, TEMPLATES, UNKNOWN_STARTS, UNKNOWN_TOKENS,
};
use crate::error::{Error, IoAction, io_error};
use crate::mapped_bytes::MappedBytes;
use crate::mapped_file::{MappedFile, open_unwritable};
use crate::section::Section;
use crate::system_dictionary::PrefixHit;
pub(crate) use crate::system_dictionary::TokenRecord;
use crate::wire::u32_at;

mod cursor;
mod scalar;
mod source;
mod token_format;
mod validate;
mod view;

pub(super) use source::Source;
pub use view::PackedView;

const CHUNK: usize = 1 << 20;
const PARAM_BYTES: usize = 8;

pub type Hit = PrefixHit;

impl std::fmt::Debug for PackedDictionary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PackedDictionary")
            .field("token_count", &self.token_count)
            .field("unknown_count", &self.unknown_count)
            .field("left_size", &self.left_size)
            .field("right_size", &self.right_size)
            .field("category_count", &self.category_count)
            .field("hot", &self.hot)
            .finish_non_exhaustive()
    }
}

pub struct PackedDictionary {
    map: Arc<MappedFile>,
    sections: [Range<usize>; SECTIONS],
    char_index: Section<u16>,
    char_blocks: Section<u16>,
    cells: Section<u8>,
    format: TokenFormat,
    token_count: usize,
    unknown_count: usize,
    left_size: usize,
    right_size: usize,
    category_count: usize,
    hot: bool,
    ascii: [u16; 128],
    bos_feature: Range<usize>,
    unknown_feature: Option<Range<usize>>,
    _specs: Arc<[NumSpec]>,
    view: FeatureView<'static>,
}

impl PackedDictionary {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let file = open_unwritable(path).map_err(io_error(IoAction::Open, path))?;
        let file_length = file
            .metadata()
            .map_err(io_error(IoAction::Inspect, path))?
            .len();
        let mut header_bytes = [0u8; HEADER_BYTES];
        if file_length < HEADER_BYTES as u64 {
            return Err(Error::format(path, "the file is shorter than its header"));
        }
        read_at(&file, &mut header_bytes, 0).map_err(io_error(IoAction::Read, path))?;
        let header = validate::parse_header(&header_bytes, path)?;
        let sections = validate::parse_sections(&header_bytes, file_length, path)?;
        let map = Arc::new(MappedFile::map(file, path)?);
        let source = Source {
            file: map.file(),
            path,
            sections: sections.clone(),
        };
        if map.len() as u64 != file_length {
            return Err(Error::format(path, "the file changed while it was opened"));
        }
        let validated = validate::run(&source, header, &map[sections[CELLS].clone()])?;
        Self::from_validated(path, map, sections, validated)
    }

    fn from_validated(
        path: &Path,
        map: Arc<MappedFile>,
        sections: [Range<usize>; SECTIONS],
        validated: validate::Validated,
    ) -> Result<Self, Error> {
        let validate::Validated {
            header,
            left_size,
            right_size,
            unknown_count,
            checked,
            dicrc,
        } = validated;
        let (features, numbers) = checked
            .into_mapped(&sections[FEATURES], map.len())
            .ok_or_else(|| Error::format(path, "the feature section is outside the file"))?;
        let view_error = || Error::format(path, "a section is misaligned");
        let char_index = Section::new(&map, sections[CHAR_INDEX].clone()).ok_or_else(view_error)?;
        let char_blocks =
            Section::new(&map, sections[CHAR_BLOCKS].clone()).ok_or_else(view_error)?;
        let cells = Section::new(&map, sections[CELLS].clone()).ok_or_else(view_error)?;
        let dicrc_start = sections[DICRC].start + validate::DICRC_HEADER_BYTES;
        let (bos_length, unknown_length) = (dicrc.bos_length, dicrc.unknown_length);
        let numbers: Arc<[NumSpec]> = numbers.into();
        let view = unsafe { FeatureView::new(&map[..], &features, &numbers).detached() };
        let mut dictionary = Self {
            map,
            char_index,
            char_blocks,
            cells,
            format: header.format,
            token_count: header.token_count,
            unknown_count,
            left_size,
            right_size,
            category_count: header.category_count,
            hot: header.hot(),
            bos_feature: dicrc_start..dicrc_start + bos_length,
            unknown_feature: (unknown_length > 0)
                .then(|| dicrc_start + bos_length..dicrc_start + bos_length + unknown_length),
            ascii: [0; 128],
            _specs: numbers,
            view,
            sections,
        };
        dictionary.ascii = std::array::from_fn(|byte| dictionary.code_of(char::from(byte as u8)));
        Ok(dictionary)
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    #[inline]
    pub fn code_of(&self, char: char) -> u16 {
        let scalar = char as usize;
        let index = self.char_index.view(&self.map);
        let blocks = self.char_blocks.view(&self.map);
        let block = usize::from(index[scalar / BLOCK_CODES]);
        blocks[block * BLOCK_CODES + scalar % BLOCK_CODES]
    }

    pub fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<Hit>) {
        self.view().common_prefix_search(key, hits);
    }

    pub fn category_count(&self) -> usize {
        self.category_count
    }

    pub fn unknown_tokens(&self, category: usize) -> impl ExactSizeIterator<Item = TokenRecord> {
        let starts = &self.map[self.sections[UNKNOWN_STARTS].clone()];
        let range = if category < self.category_count {
            u32_at(starts, category * 4) as usize..u32_at(starts, category * 4 + 4) as usize
        } else {
            0..0
        };
        let bytes = &self.map[self.sections[UNKNOWN_TOKENS].clone()];
        let view = self.view();
        let count = self.unknown_count;
        range.map(move |index| view.token_at(bytes, count, index))
    }

    #[inline]
    pub(crate) fn feature_view(&self) -> &FeatureView<'_> {
        &self.view
    }

    #[inline]
    pub fn write_feature(&self, offset: u32, key: &str, out: &mut String) {
        self.feature_view()
            .emit(offset, key, out)
            .expect("writing to a string does not fail");
    }

    pub fn key_of<'t>(&self, text: &'t str, offset: u32) -> &'t str {
        let mut hits = Vec::new();
        self.common_prefix_search(text.as_bytes(), &mut hits);
        let view = self.view();
        hits.iter()
            .find(|hit| {
                hit.tokens()
                    .any(|index| view.token(index).feature == offset)
            })
            .map_or("", |hit| &text[..hit.length()])
    }

    pub(crate) fn matrix_mapped(&self) -> MappedBytes {
        MappedBytes::from_arc(Arc::clone(&self.map), self.sections[MATRIX].clone())
            .expect("the matrix section is inside the mapping")
    }

    pub(crate) fn templates_mapped(&self) -> Option<MappedBytes> {
        self.hot.then(|| {
            MappedBytes::from_arc(Arc::clone(&self.map), self.sections[TEMPLATES].clone())
                .expect("the template section is inside the mapping")
        })
    }

    pub(crate) fn char_table_mapped(&self) -> MappedBytes {
        MappedBytes::from_arc(Arc::clone(&self.map), self.sections[CHAR_TABLE].clone())
            .expect("the character-table section is inside the mapping")
    }

    pub fn bos_feature(&self) -> &str {
        unsafe { str::from_utf8_unchecked(&self.map[self.bos_feature.clone()]) }
    }

    pub fn unknown_feature(&self) -> Option<&str> {
        self.unknown_feature
            .clone()
            .map(|range| unsafe { str::from_utf8_unchecked(&self.map[range]) })
    }
}
