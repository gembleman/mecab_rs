use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

use super::bits::{MAX_RECORD_BITS, mask_of, section_bytes};
use super::features::{FeatureView, Layout as FeatureLayout, NumSpec, check as feature_check};
use super::{
    BLOCK_CODES, BLOCK_COUNT, CELLS, CELLS_PADDING, CHAR_BLOCKS, CHAR_INDEX, CHAR_TABLE, DICRC,
    FEATURES, FLAG_HOT_MATRIX, FLAG_INT_TEMPLATES, FLAGS_SHIFT, HEADER_BYTES, INLINE_COUNT_MASK,
    INLINE_COUNT_SHIFT, INLINE_FLAG, MAGIC, MATRIX, MAX_UNITS, PARAMS, SECTION_NAMES, SECTIONS,
    TEMPLATES, TERMINAL_FLAG, TOKENS, UNIT_BYTES, UNKNOWN_STARTS, UNKNOWN_TOKENS, UNUSED_LABEL,
    VALUE_MASK, VERSION, label_of,
};
use crate::char_table::{HEADER_BYTES as CHAR_TABLE_HEADER, MAGIC as CHAR_TABLE_MAGIC};
use crate::dictionary::map_file;
use crate::error::Error;
use crate::hot_shape::HotShape;
use crate::int_runtime::is_runtime_form;
use crate::mapped_bytes::MappedBytes;
use crate::section::Section;
use crate::system_dictionary::PrefixHit;
pub use crate::system_dictionary::TokenRecord;

const CHUNK: usize = 1 << 20;
const PARAM_BYTES: usize = 8;
const MAGIC_BYTES: usize = 8;
const SECTION_TABLE_AT: usize = 40;
const SECTION_ENTRY_BYTES: usize = 16;
const MATRIX_HEADER_BYTES: usize = 4;
const HOT_ROWS_BYTES: usize = 4;
const ROW_START_BYTES: usize = 4;

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

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}

pub(super) struct Source<'a> {
    file: &'a File,
    path: &'a Path,
    sections: [Range<usize>; SECTIONS],
}

impl<'a> Source<'a> {
}

impl Source<'_> {
    pub(super) fn broken(&self, reason: &'static str) -> Error {
        Error::Format {
            file: self.path.to_path_buf(),
            reason,
        }
    }

    pub(super) fn section_length(&self, section: usize) -> usize {
        self.sections[section].len()
    }

    pub(super) fn read(&self, section: usize, from: usize, buffer: &mut [u8]) -> Result<(), Error> {
        let start = self.sections[section].start + from;
        read_at(self.file, buffer, start as u64).map_err(|source| Error::Io {
            path: self.path.to_path_buf(),
            source,
        })
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
    integer_templates: bool,
    ascii: [u16; 128],
    bos_feature: Range<usize>,
    unknown_feature: Option<Range<usize>>,
    features: FeatureLayout,
    numbers: Box<[NumSpec]>,
}

impl PackedDictionary {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let io_error = |source| Error::Io {
            path: path.to_path_buf(),
            source,
        };
        let broken = |reason| Error::Format {
            file: path.to_path_buf(),
            reason,
        };
        let file = File::open(path).map_err(io_error)?;
        let file_length = file.metadata().map_err(io_error)?.len();
        let mut header = [0u8; HEADER_BYTES];
        if file_length < HEADER_BYTES as u64 {
            return Err(broken("the file is shorter than its header"));
        }
        read_at(&file, &mut header, 0).map_err(io_error)?;
        if header[..MAGIC_BYTES] != *MAGIC {
            return Err(broken("the magic number is not valid"));
        }
        if u32_at(&header, 8) != VERSION {
            return Err(broken(
                "the file has another format version; pack the dictionary again",
            ));
        }
        let flags = u32_at(&header, 12);
        if flags & !(FLAG_HOT_MATRIX | FLAG_INT_TEMPLATES) != 0 {
            return Err(broken("the flags are not valid"));
        }
        if flags & FLAG_INT_TEMPLATES != 0 && flags & FLAG_HOT_MATRIX == 0 {
            return Err(broken("integer templates require a hot matrix"));
        }
        let code_count = u32_at(&header, 16) as usize;
        let token_count = u32_at(&header, 20) as usize;
        let param_count = u32_at(&header, 24) as usize;
        let category_count = u32_at(&header, 28) as usize;
        let (param_bits, feature_bits) = (u32_at(&header, 32), u32_at(&header, 36));
        if param_bits == 0 || feature_bits == 0 || param_bits > 32 || feature_bits > 32 {
            return Err(broken("the widths of the token fields are not valid"));
        }
        let format = TokenFormat::new(param_bits, feature_bits);
        if format.bits > MAX_RECORD_BITS as usize {
            return Err(broken("the widths of the token fields are not valid"));
        }
        let mut sections: [Range<usize>; SECTIONS] = Default::default();
        let mut end = HEADER_BYTES as u64;
        for (number, section) in sections.iter_mut().enumerate() {
            let at = SECTION_TABLE_AT + SECTION_ENTRY_BYTES * number;
            let (offset, length) = (u64_at(&header, at), u64_at(&header, at + 8));
            let stop = offset.checked_add(length);
            if offset % 8 != 0 || offset < end || stop.is_none_or(|stop| stop > file_length) {
                return Err(broken("a section is misplaced or outside the file"));
            }
            end = offset + length;
            *section = offset as usize..(offset + length) as usize;
        }
        let source = Source {
            file: &file,
            path,
            sections: sections.clone(),
        };
        let map = Arc::new(map_file(path)?);
        if map.len() as u64 != file_length {
            return Err(broken("the file changed while it was opened"));
        }
        let length = |section: usize| sections[section].len();
        if length(CHAR_INDEX) != BLOCK_COUNT * 2
            || length(CHAR_BLOCKS) == 0
            || length(CHAR_BLOCKS) % (BLOCK_CODES * 2) != 0
            || length(CELLS) < UNIT_BYTES + CELLS_PADDING
            || !(length(CELLS) - CELLS_PADDING).is_multiple_of(UNIT_BYTES)
            || (length(CELLS) - CELLS_PADDING) / UNIT_BYTES > MAX_UNITS
            || length(TOKENS) as u64 != section_bytes(token_count, format.bits)
            || length(PARAMS) != param_count * PARAM_BYTES
            || length(UNKNOWN_STARTS) != (category_count + 1) * 4
            || code_count == 0
            || code_count > 1 << 16
            || token_count > MAX_UNITS
        {
            return Err(broken("the size of a section does not match the header"));
        }

        let mut matrix_header = [0u8; MATRIX_HEADER_BYTES];
        if length(MATRIX) < MATRIX_HEADER_BYTES {
            return Err(broken("the matrix is shorter than its header"));
        }
        source.read(MATRIX, 0, &mut matrix_header)?;
        let left_size = usize::from(u16_at(&matrix_header, 0));
        let right_size = usize::from(u16_at(&matrix_header, 2));
        let hot = flags & FLAG_HOT_MATRIX != 0;
        let integer_templates = flags & FLAG_INT_TEMPLATES != 0;
        let expected_length = if hot {
            let mut hot_header = [0u8; HOT_ROWS_BYTES];
            source.read(MATRIX, MATRIX_HEADER_BYTES, &mut hot_header)?;
            let rows = u32_at(&hot_header, 0) as usize;
            if rows == 0 || rows > right_size {
                return Err(broken("the hot block lies outside of the matrix"));
            }
            let mut last_start = [0u8; ROW_START_BYTES];
            source.read(
                MATRIX,
                HotShape::header_bytes(rows) - ROW_START_BYTES,
                &mut last_start,
            )?;
            let cells = u32::from_le_bytes(last_start) as usize;
            HotShape::header_bytes(rows) + cells * 2
        } else {
            MATRIX_HEADER_BYTES + left_size * right_size * 2
        };
        if left_size == 0 || right_size == 0 || length(MATRIX) != expected_length {
            return Err(broken("the matrix size does not match its header"));
        }
        if hot && integer_templates {
            let mut templates_header = [0u8; 24];
            if length(TEMPLATES) < templates_header.len() {
                return Err(broken("the integer templates are empty"));
            }
            source.read(TEMPLATES, 0, &mut templates_header)?;
            if !is_runtime_form(&templates_header)
                || u32_at(&templates_header, 16) as usize != left_size
                || u32_at(&templates_header, 20) as usize != right_size
            {
                return Err(broken("the integer templates do not match the hot matrix"));
            }
        } else if hot {
            let mut templates_header = [0u8; 24];
            if length(TEMPLATES) < templates_header.len() {
                return Err(broken("the templates are shorter than their header"));
            }
            source.read(TEMPLATES, 0, &mut templates_header)?;
            if templates_header[..4] != *b"MXTP"
                || u32_at(&templates_header, 16) as usize != left_size
                || u32_at(&templates_header, 20) as usize != right_size
            {
                return Err(broken("the templates do not match the hot matrix"));
            }
        } else if length(TEMPLATES) != 0 {
            return Err(broken("a dense matrix comes with templates"));
        }
        if length(CHAR_TABLE) < CHAR_TABLE_HEADER {
            return Err(broken("the character table is shorter than its header"));
        }
        let mut table_header = vec![0u8; CHAR_TABLE_HEADER];
        source.read(CHAR_TABLE, 0, &mut table_header)?;
        if table_header[..4] != *CHAR_TABLE_MAGIC
            || u32_at(&table_header, 8) as usize != category_count
        {
            return Err(broken(
                "the character table is not valid or has other categories than the unknown words",
            ));
        }

        let index = source.read_all(CHAR_INDEX)?;
        let block_count = length(CHAR_BLOCKS) / (BLOCK_CODES * 2);
        if index
            .as_chunks::<2>()
            .0
            .iter()
            .any(|&pair| usize::from(u16::from_le_bytes(pair)) >= block_count)
        {
            return Err(broken("a character block does not exist"));
        }
        source.chunks(
            CHAR_BLOCKS,
            2,
            |_, part| {
                part.as_chunks::<2>()
                    .0
                    .iter()
                    .all(|&pair| usize::from(u16::from_le_bytes(pair)) < code_count)
            },
            "a character code is out of range",
        )?;

        let checked = feature_check::check(&source, FEATURES)?;

        source.chunks(
            PARAMS,
            PARAM_BYTES,
            |_, part| {
                part.as_chunks::<PARAM_BYTES>().0.iter().all(|param| {
                    usize::from(u16_at(param, 0)) < right_size
                        && usize::from(u16_at(param, 2)) < left_size
                })
            },
            "a parameter set has a context id outside the matrix",
        )?;

        let unknown_starts = source.read_all(UNKNOWN_STARTS)?;
        let unknown_starts: Vec<u32> = unknown_starts
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&word| u32::from_le_bytes(word))
            .collect();
        if unknown_starts[0] != 0 || unknown_starts.windows(2).any(|pair| pair[0] > pair[1]) {
            return Err(broken("the unknown tokens of the categories do not add up"));
        }
        let unknown_count = unknown_starts[category_count] as usize;
        if length(UNKNOWN_TOKENS) as u64 != section_bytes(unknown_count, format.bits) {
            return Err(broken("the unknown tokens of the categories do not add up"));
        }

        let valid_token = |param: u64, feature: u64| {
            param < param_count as u64 && checked.is_record(feature as usize)
        };
        let reason = "a token has a parameter set or a feature that does not exist";
        check_tokens(&source, TOKENS, token_count, format, valid_token, reason)?;
        check_tokens(
            &source,
            UNKNOWN_TOKENS,
            unknown_count,
            format,
            valid_token,
            reason,
        )?;

        check_cells(&source, &map[sections[CELLS].clone()], token_count)?;

        let dicrc = source.read_all(DICRC)?;
        if dicrc.len() < 8 {
            return Err(broken("dicrc is shorter than its header"));
        }
        let bos_length = u32_at(&dicrc, 0) as usize;
        let unknown_length = u32_at(&dicrc, 4) as usize;
        if bos_length == 0 || 8 + bos_length + unknown_length != dicrc.len() {
            return Err(broken("the bos-feature or unk-feature length is not valid"));
        }
        if str::from_utf8(&dicrc[8..]).is_err()
            || str::from_utf8(&dicrc[8..8 + bos_length]).is_err()
        {
            return Err(broken("bos-feature or unk-feature is not valid UTF-8"));
        }

        let view_error = || broken("a section is misaligned");
        let char_index = Section::new(&map, sections[CHAR_INDEX].clone()).ok_or_else(view_error)?;
        let char_blocks =
            Section::new(&map, sections[CHAR_BLOCKS].clone()).ok_or_else(view_error)?;
        let cells = Section::new(&map, sections[CELLS].clone()).ok_or_else(view_error)?;
        let dicrc_start = sections[DICRC].start + 8;
        let mut dictionary = Self {
            map,
            char_index,
            char_blocks,
            cells,
            format,
            token_count,
            unknown_count,
            left_size,
            right_size,
            category_count,
            hot: flags & FLAG_HOT_MATRIX != 0,
            integer_templates,
            bos_feature: dicrc_start..dicrc_start + bos_length,
            unknown_feature: (unknown_length > 0)
                .then(|| dicrc_start + bos_length..dicrc_start + bos_length + unknown_length),
            ascii: [0; 128],
            features: checked.layout.moved(sections[FEATURES].start),
            numbers: checked.specs.into(),
            sections,
        };
        dictionary.ascii = std::array::from_fn(|byte| dictionary.code_of(char::from(byte as u8)));
        Ok(dictionary)
    }

    pub fn token_count(&self) -> usize {
        self.token_count
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn section_lengths(&self) -> Vec<(&'static str, usize)> {
        SECTION_NAMES
            .iter()
            .zip(&self.sections)
            .map(|(&name, range)| (name, range.len()))
            .collect()
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

    pub fn common_prefix_search_codes(&self, codes: &[u16], hits: &mut Vec<Hit>) {
        hits.clear();
        let mut cursor = Cursor::new(self.cells.view(&self.map));
        for (at, &code) in codes.iter().enumerate() {
            match cursor.step(code) {
                Step::Stop => return,
                Step::Inner => {}
                Step::Key(first_token, token_count) => hits.push(Hit {
                    length: at + 1,
                    first_token: first_token as usize,
                    token_count: token_count as usize,
                }),
            }
        }
    }

    pub fn exact_match(&self, key: &[u8]) -> Option<Range<usize>> {
        let mut found = None;
        self.view().walk_bytes(key, |hit| found = Some(hit));
        found.filter(|hit| hit.length == key.len()).map(Hit::tokens)
    }

    #[inline]
    pub fn token(&self, index: usize) -> TokenRecord {
        self.view().token(index)
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
        let layout = &self.features;
        let map = &self.map[..];
        let part = |range: &Range<usize>| unsafe { map.get_unchecked(range.start..range.end) };
        FeatureView {
            positions: part(&layout.positions),
            piece_offsets: part(&layout.piece_offsets),
            pool: part(&layout.pool),
            entries: part(&layout.entries),
            entry_bits: layout.counts.entry_bits,
            shape_offsets: part(&layout.shape_offsets),
            shapes: part(&layout.shapes),
            lo: part(&layout.lo),
            chars: part(&layout.chars),
            strings: part(&layout.strings),
            blob: part(&layout.blob),
            template_groups: part(&layout.template_groups),
            group_starts: part(&layout.group_starts),
            rule_entries: part(&layout.rule_entries),
            rules: part(&layout.rules),
            records: part(&layout.records),
            specs: &self.numbers,
            template_bits: layout.template_bits,
            shape_bits: layout.shape_bits,
            ref_bits: layout.ref_bits,
            blob_bits: layout.blob_bits,
            id_bytes: layout.counts.id_bytes as usize,
        }
    }

    #[inline]
    pub fn write_feature(&self, offset: u32, key: &str, out: &mut String) {
        self.feature_view()
            .emit(offset, key, out)
            .expect("writing to a string does not fail");
    }

    pub fn feature_string(&self, offset: u32, key: &str) -> String {
        let mut out = String::new();
        self.write_feature(offset, key, &mut out);
        out
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

    pub fn matrix_bytes(&self) -> &[u8] {
        &self.map[self.sections[MATRIX].clone()]
    }

    pub(crate) fn matrix_mapped(&self) -> MappedBytes {
        MappedBytes::from_arc(Arc::clone(&self.map), self.sections[MATRIX].clone())
            .expect("the matrix section is inside the mapping")
    }

    pub fn template_bytes(&self) -> Option<&[u8]> {
        self.hot
            .then(|| &self.map[self.sections[TEMPLATES].clone()])
    }

    pub(crate) fn templates_mapped(&self) -> Option<MappedBytes> {
        self.hot.then(|| {
            MappedBytes::from_arc(Arc::clone(&self.map), self.sections[TEMPLATES].clone())
                .expect("the template section is inside the mapping")
        })
    }

    pub fn integer_templates(&self) -> bool {
        self.integer_templates
    }

    pub fn char_table_bytes(&self) -> &[u8] {
        &self.map[self.sections[CHAR_TABLE].clone()]
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

const NO_CHILDREN: usize = 1 << 25;

#[inline(always)]
fn unit_at(cells: &[u8], index: usize) -> Option<u64> {
    let bytes = cells.get(index * UNIT_BYTES..index * UNIT_BYTES + 8)?;
    Some(u64::from_le_bytes(*bytes.first_chunk()?))
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
            base: unit_at(cells, 0).map_or(NO_CHILDREN, |root| (root & VALUE_MASK) as usize),
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
        if label_of(unit) != code {
            return Step::Stop;
        }
        let value = (unit & VALUE_MASK) as u32;
        let flags = (unit >> FLAGS_SHIFT) as u8;
        self.base = value as usize;
        if flags & TERMINAL_FLAG == 0 {
            Step::Inner
        } else if flags & INLINE_FLAG != 0 {
            self.base = NO_CHILDREN;
            Step::Key(
                value,
                u32::from((flags >> INLINE_COUNT_SHIFT) & INLINE_COUNT_MASK),
            )
        } else if let Some(data) = unit_at(self.cells, value as usize) {
            Step::Key(
                (data & VALUE_MASK) as u32,
                (data >> FLAGS_SHIFT) as u8 as u32,
            )
        } else {
            Step::Stop
        }
    }
}

#[inline]
fn decode(bytes: &[u8]) -> Option<(char, usize)> {
    let &first = bytes.first()?;
    if first < 0x80 {
        return Some((char::from(first), 1));
    }
    let (width, minimum) = match first {
        0xC0..=0xDF => (2, 0x80),
        0xE0..=0xEF => (3, 0x800),
        0xF0..=0xF7 => (4, 0x1_0000),
        _ => return None,
    };
    let tail = bytes.get(1..width)?;
    let mut scalar = u32::from(first) & (0x7F >> width);
    for &byte in tail {
        if byte & 0xC0 != 0x80 {
            return None;
        }
        scalar = scalar << 6 | u32::from(byte & 0x3F);
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

    #[inline(always)]
    fn walk_bytes(&self, key: &[u8], mut found: impl FnMut(Hit)) {
        let mut cursor = Cursor::new(self.cells);
        let mut at = 0;
        while let Some(&first) = key.get(at) {
            let (code, width) = if first < 0x80 {
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
            let param = (word & format.param_mask) as usize;
            let feature = ((word >> format.param_bits) & format.feature_mask) as u32;
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
            let param = word & format.param_mask;
            let feature = (word >> format.param_bits) & format.feature_mask;
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
        u64::from_le_bytes([a, b, c, d, e, f, 0, 0])
    };
    for chunk in units {
        let unit = decode(chunk);
        let label = label_of(unit);
        let value = (unit & VALUE_MASK) as u32;
        let flags = (unit >> FLAGS_SHIFT) as u8;
        let valid = if label == UNUSED_LABEL {
            true
        } else if label == 0 {
            valid_range(value, usize::from(flags))
        } else if flags & TERMINAL_FLAG == 0 {
            flags == 0
        } else if flags & INLINE_FLAG != 0 {
            flags >> 7 == 0
                && valid_range(
                    value,
                    usize::from((flags >> INLINE_COUNT_SHIFT) & INLINE_COUNT_MASK),
                )
        } else {
            flags == TERMINAL_FLAG
                && units
                    .get(value as usize)
                    .is_some_and(|data| label_of(decode(data)) == 0)
        };
        if !valid {
            return Err(source.broken(reason));
        }
    }
    Ok(())
}
