use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

use super::bits::{MAX_RECORD_BITS, mask_of, section_bytes};
use super::features::check::MappedLayout;
use super::features::{FeatureView, NumSpec, check as feature_check};
use super::{
    BLOCK_CODES, BLOCK_COUNT, CATEGORY_COUNT_AT, CELLS, CELLS_PADDING, CHAR_BLOCKS, CHAR_INDEX,
    CHAR_TABLE, CODE_COUNT_AT, DATA_LABEL, DICRC, FEATURE_BITS_AT, FEATURES, FLAG_HOT_MATRIX,
    FLAG_INT_TEMPLATES, FLAGS_AT, HEADER_BYTES, MAGIC, MAGIC_BYTES, MATRIX, MAX_UNITS,
    PARAM_BITS_AT, PARAM_COUNT_AT, PARAMS, SECTION_ALIGNMENT, SECTION_ENTRY_BYTES,
    SECTION_LENGTH_IN_ENTRY, SECTION_OFFSET_IN_ENTRY, SECTION_TABLE_AT, SECTIONS, TEMPLATES,
    TERMINAL_FLAG, TOKEN_COUNT_AT, TOKENS, UNIT_BYTES, UNKNOWN_STARTS, UNKNOWN_TOKENS,
    UNUSED_LABEL, Unit, VALUE_BITS, VERSION, VERSION_AT,
};
use crate::char_property::SentenceChars;
use crate::char_table::{CHAR_TABLE_HEADER_BYTES, MAGIC as CHAR_TABLE_MAGIC};
use crate::dictionary::{map_open_file, open_unwritable};
use crate::error::{Error, io_error};
use crate::hot_shape::HotShape;
use crate::int_runtime::is_runtime_form;
use crate::mapped_bytes::MappedBytes;
use crate::matrix::MATRIX_HEADER_BYTES;
use crate::section::Section;
use crate::system_dictionary::PrefixHit;
pub(crate) use crate::system_dictionary::TokenRecord;
use crate::templates::{TEMPLATE_HEADER_PEEK_BYTES, header_sizes};
use crate::utf8::is_continuation;
use crate::wire::{u16_at, u16_values, u32_at, u32_values, u64_at};

const CHUNK: usize = 1 << 20;
const PARAM_BYTES: usize = 8;
const HOT_ROWS_BYTES: usize = 4;
const ROW_START_BYTES: usize = 4;

mod validate;

pub type Hit = PrefixHit;

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        let read = std::os::windows::fs::FileExt::seek_read(file, buffer, offset)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer = &mut buffer[read..];
        offset += read as u64;
    }
    Ok(())
}

pub(super) struct Source<'a> {
    file: &'a File,
    path: &'a Path,
    sections: [Range<usize>; SECTIONS],
}

impl Source<'_> {
    pub(super) fn broken(&self, reason: &'static str) -> Error {
        Error::format(self.path, reason)
    }

    pub(super) fn section_length(&self, section: usize) -> usize {
        self.sections[section].len()
    }

    pub(super) fn read(&self, section: usize, from: usize, buffer: &mut [u8]) -> Result<(), Error> {
        let start = self.sections[section].start + from;
        read_at(self.file, buffer, start as u64).map_err(io_error(self.path))
    }

    fn read_all(&self, section: usize) -> Result<Vec<u8>, Error> {
        let mut bytes = vec![0; self.sections[section].len()];
        self.read(section, 0, &mut bytes)?;
        Ok(bytes)
    }

    fn chunks(
        &self,
        section: usize,
        unit: usize,
        mut visit: impl FnMut(usize, &[u8]) -> bool,
        reason: &'static str,
    ) -> Result<(), Error> {
        let length = self.sections[section].len();
        let step = CHUNK / unit * unit;
        let mut buffer = vec![0; step.min(length)];
        let mut done = 0;
        while done < length {
            let part = step.min(length - done);
            self.read(section, done, &mut buffer[..part])?;
            if !visit(done / unit, &buffer[..part]) {
                return Err(self.broken(reason));
            }
            done += part;
        }
        Ok(())
    }
}

pub struct PackedDictionary {
    map: Arc<Mmap>,
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
    features: MappedLayout,
    numbers: Box<[NumSpec]>,
}

impl PackedDictionary {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let file = open_unwritable(path).map_err(io_error(path))?;
        let file_length = file.metadata().map_err(io_error(path))?.len();
        let mut header_bytes = [0u8; HEADER_BYTES];
        if file_length < HEADER_BYTES as u64 {
            return Err(Error::format(path, "the file is shorter than its header"));
        }
        read_at(&file, &mut header_bytes, 0).map_err(io_error(path))?;
        let header = validate::parse_header(&header_bytes, path)?;
        let sections = validate::parse_sections(&header_bytes, file_length, path)?;
        let source = Source {
            file: &file,
            path,
            sections: sections.clone(),
        };
        let map = Arc::new(map_open_file(&file, path)?);
        if map.len() as u64 != file_length {
            return Err(Error::format(path, "the file changed while it was opened"));
        }
        let validated = validate::run(&source, header, &map[sections[CELLS].clone()])?;
        Self::from_validated(path, map, sections, validated)
    }

    fn from_validated(
        path: &Path,
        map: Arc<Mmap>,
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
            features,
            numbers: numbers.into(),
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
    pub fn view(&self) -> PackedView<'_> {
        PackedView {
            char_index: self.char_index.view(&self.map),
            char_blocks: self.char_blocks.view(&self.map),
            cells: self.cells.view(&self.map),
            tokens: &self.map[self.sections[TOKENS].clone()],
            token_count: self.token_count,
            params: &self.map[self.sections[PARAMS].clone()],
            format: self.format,
            ascii: &self.ascii,
        }
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
    pub(crate) fn feature_view(&self) -> FeatureView<'_> {
        FeatureView::new(&self.map[..], &self.features, &self.numbers)
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

enum Step {
    Stop,
    Inner,
    Key(u32, u32),
}

const NO_CHILDREN: usize = 1 << (VALUE_BITS + 1);

#[inline(always)]
fn unit_at(cells: &[u8], index: usize) -> Option<Unit> {
    let bytes = cells.get(index * UNIT_BYTES..index * UNIT_BYTES + 8)?;
    Some(Unit::from_bits(u64::from_le_bytes(*bytes.first_chunk()?)))
}

struct Cursor<'a> {
    cells: &'a [u8],
    base: usize,
}

impl<'a> Cursor<'a> {
    #[inline(always)]
    fn new(cells: &'a [u8]) -> Self {
        Self {
            cells,
            base: unit_at(cells, 0).map_or(NO_CHILDREN, |root| root.value() as usize),
        }
    }

    #[inline(always)]
    fn step(&mut self, code: u16) -> Step {
        if code == 0 {
            return Step::Stop;
        }
        let Some(unit) = unit_at(self.cells, self.base + usize::from(code)) else {
            return Step::Stop;
        };
        if unit.label() != code {
            return Step::Stop;
        }
        let value = unit.value();
        self.base = value as usize;
        if !unit.is_terminal() {
            Step::Inner
        } else if unit.is_inline() {
            self.base = NO_CHILDREN;
            Step::Key(value, u32::from(unit.inline_count()))
        } else if let Some(data) = unit_at(self.cells, value as usize) {
            Step::Key(data.value(), u32::from(data.flags()))
        } else {
            Step::Stop
        }
    }
}

const ASCII_LIMIT: u8 = 0x80;
const LEAD_PAYLOAD_MASK: u8 = 0x7F;
const CONTINUATION_PAYLOAD_BITS: u32 = 6;
const CONTINUATION_PAYLOAD_MASK: u8 = (1 << CONTINUATION_PAYLOAD_BITS) - 1;

#[inline(always)]
fn lead_payload_mask(width: usize) -> u32 {
    u32::from(LEAD_PAYLOAD_MASK >> width)
}

#[inline]
fn decode(bytes: &[u8]) -> Option<(char, usize)> {
    let &first = bytes.first()?;
    if first < ASCII_LIMIT {
        return Some((char::from(first), 1));
    }
    let (width, minimum) = match first {
        0xC0..=0xDF => (2, 0x80),
        0xE0..=0xEF => (3, 0x800),
        0xF0..=0xF7 => (4, 0x1_0000),
        _ => return None,
    };
    let tail = bytes.get(1..width)?;
    let mut scalar = u32::from(first) & lead_payload_mask(width);
    for &byte in tail {
        if !is_continuation(byte) {
            return None;
        }
        scalar = scalar << CONTINUATION_PAYLOAD_BITS | u32::from(byte & CONTINUATION_PAYLOAD_MASK);
    }
    if scalar < minimum {
        return None;
    }
    char::from_u32(scalar).map(|char| (char, width))
}

#[derive(Clone, Copy)]
pub struct PackedView<'a> {
    char_index: &'a [u16],
    char_blocks: &'a [u16],
    cells: &'a [u8],
    tokens: &'a [u8],
    token_count: usize,
    params: &'a [u8],
    format: TokenFormat,
    ascii: &'a [u16; 128],
}

impl PackedView<'_> {
    #[inline]
    fn code_of(&self, char: char) -> u16 {
        let scalar = char as usize;
        let block = usize::from(self.char_index[scalar / BLOCK_CODES]);
        self.char_blocks[block * BLOCK_CODES + scalar % BLOCK_CODES]
    }

    #[inline]
    pub fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<Hit>) {
        hits.clear();
        self.walk_bytes(key, |hit| hits.push(hit));
    }

    #[inline]
    pub(crate) fn trie_code_of(&self, scalar: u32) -> u16 {
        match u8::try_from(scalar) {
            Ok(byte) if byte < ASCII_LIMIT => self.ascii[usize::from(byte)],
            _ => {
                let scalar = scalar as usize;
                let block = usize::from(self.char_index[scalar / BLOCK_CODES]);
                self.char_blocks[block * BLOCK_CODES + scalar % BLOCK_CODES]
            }
        }
    }

    #[inline]
    pub(crate) fn common_prefix_search_sentence(
        &self,
        chars: &SentenceChars,
        start: usize,
        hits: &mut Vec<Hit>,
    ) {
        hits.clear();
        let mut cursor = Cursor::new(self.cells);
        let length = chars.len();
        let mut position = start;
        while position < length {
            let (code, width) = chars.trie_step(position);
            position += width;
            match cursor.step(code) {
                Step::Stop => return,
                Step::Inner => {}
                Step::Key(first_token, token_count) => hits.push(Hit {
                    length: position - start,
                    first_token: first_token as usize,
                    token_count: token_count as usize,
                }),
            }
        }
    }

    #[inline(always)]
    fn walk_bytes(&self, key: &[u8], mut found: impl FnMut(Hit)) {
        let mut cursor = Cursor::new(self.cells);
        let mut at = 0;
        while let Some(&first) = key.get(at) {
            let (code, width) = if first < ASCII_LIMIT {
                (self.ascii[usize::from(first)], 1)
            } else {
                let Some((char, width)) = decode(&key[at..]) else {
                    return;
                };
                (self.code_of(char), width)
            };
            at += width;
            match cursor.step(code) {
                Step::Stop => return,
                Step::Inner => {}
                Step::Key(first_token, token_count) => found(Hit {
                    length: at,
                    first_token: first_token as usize,
                    token_count: token_count as usize,
                }),
            }
        }
    }

    #[inline]
    pub fn token(&self, index: usize) -> TokenRecord {
        self.token_at(self.tokens, self.token_count, index)
    }

    #[inline]
    pub fn tokens(&self, range: Range<usize>) -> impl Iterator<Item = TokenRecord> {
        assert!(
            range.start <= range.end && range.end <= self.token_count,
            "token range out of range"
        );
        let view = *self;
        range.map(move |index| unsafe { view.token_unchecked(view.tokens, index) })
    }

    #[inline]
    fn token_at(&self, bytes: &[u8], count: usize, index: usize) -> TokenRecord {
        assert!(index < count, "token index out of range");
        unsafe { self.token_unchecked(bytes, index) }
    }

    #[inline(always)]
    unsafe fn token_unchecked(&self, bytes: &[u8], index: usize) -> TokenRecord {
        let format = self.format;
        let bit = index * format.bits;
        unsafe {
            let word = u64::from_le(bytes.as_ptr().add(bit >> 3).cast::<u64>().read_unaligned())
                >> (bit & 7);
            let (param, feature) = format.split(word);
            let (param, feature) = (param as usize, feature as u32);
            let record = self.params.as_ptr().add(param * PARAM_BYTES);
            let word = |at: usize| u16::from_le(record.add(at).cast::<u16>().read_unaligned());
            TokenRecord {
                left_id: word(0),
                right_id: word(2),
                pos_id: word(4),
                word_cost: word(6).cast_signed(),
                feature,
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TokenFormat {
    param_bits: u32,
    bits: usize,
    param_mask: u64,
    feature_mask: u64,
}

impl TokenFormat {
    #[inline(always)]
    fn split(self, word: u64) -> (u64, u64) {
        (
            word & self.param_mask,
            (word >> self.param_bits) & self.feature_mask,
        )
    }

    fn new(param_bits: u32, feature_bits: u32) -> Self {
        Self {
            param_bits,
            bits: (param_bits + feature_bits) as usize,
            param_mask: mask_of(param_bits),
            feature_mask: mask_of(feature_bits),
        }
    }
}

fn check_tokens(
    source: &Source,
    section: usize,
    count: usize,
    format: TokenFormat,
    valid: impl Fn(u64, u64) -> bool,
    reason: &'static str,
) -> Result<(), Error> {
    let bits = format.bits;
    let step = ((CHUNK * 8 / bits) & !7).max(8);
    let mut buffer = vec![0u8; (step * bits).div_ceil(8) + 8];
    let mut done = 0;
    while done < count {
        let tokens = step.min(count - done);
        let bytes = (tokens * bits).div_ceil(8);
        source.read(section, done * bits / 8, &mut buffer[..bytes])?;
        buffer[bytes..].fill(0);
        for token in 0..tokens {
            let at = token * bits;
            let word = buffer
                .get(at >> 3..)
                .and_then(|rest| rest.first_chunk())
                .map_or(0, |&word| u64::from_le_bytes(word))
                >> (at & 7);
            let (param, feature) = format.split(word);
            if !valid(param, feature) {
                return Err(source.broken(reason));
            }
        }
        done += tokens;
    }
    Ok(())
}

fn check_cells(source: &Source, bytes: &[u8], token_count: usize) -> Result<(), Error> {
    let units = bytes.as_chunks::<UNIT_BYTES>().0;
    let reason = "a double array unit is not valid";
    let valid_range = |first: u32, count: usize| count > 0 && first as usize + count <= token_count;
    let decode = |chunk: &[u8; UNIT_BYTES]| {
        let [a, b, c, d, e, f] = *chunk;
        Unit::from_bits(u64::from_le_bytes([a, b, c, d, e, f, 0, 0]))
    };
    for chunk in units {
        let unit = decode(chunk);
        let label = unit.label();
        let value = unit.value();
        let flags = unit.flags();
        let valid = if label == UNUSED_LABEL {
            true
        } else if label == DATA_LABEL {
            valid_range(value, usize::from(flags))
        } else if !unit.is_terminal() {
            flags == 0
        } else if unit.is_inline() {
            !unit.has_unused_flag_bits() && valid_range(value, usize::from(unit.inline_count()))
        } else {
            flags == TERMINAL_FLAG
                && units
                    .get(value as usize)
                    .is_some_and(|data| decode(data).label() == DATA_LABEL)
        };
        if !valid {
            return Err(source.broken(reason));
        }
    }
    Ok(())
}
