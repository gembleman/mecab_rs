
use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use memmap2::Mmap;

use super::{
    BLOCK_CODES, BLOCK_COUNT, CELLS, CHAR_BLOCKS, CHAR_INDEX, CHAR_TABLE, COUNT_SHIFT, DICRC,
    FEATURES, FLAG_HOT_MATRIX, FLAG_INT_TEMPLATES, FLAG_WIDE_PARAMS, HEADER_BYTES,
    INLINE_COUNT_MASK, INLINE_FLAG, MAGIC, MATRIX, MAX_UNITS, PARAMS, PARENT_MASK, SECTION_NAMES,
    SECTIONS, TEMPLATES, TERMINAL_FLAG, TOKENS, UNKNOWN_STARTS, UNKNOWN_TOKENS, UNUSED_CHECK,
    VERSION,
};
use crate::char_table::{HEADER_BYTES as CHAR_TABLE_HEADER, MAGIC as CHAR_TABLE_MAGIC};
use crate::dictionary::map_file;
use crate::error::Error;
use crate::int_templates::MappedIntTemplates;
use crate::mapped_bytes::MappedBytes;
use crate::section::Section;
use crate::system_dictionary::PrefixHit;
pub use crate::system_dictionary::TokenRecord;

const CHUNK: usize = 1 << 20;
const PARAM_BYTES: usize = 8;
const PARALLEL_MIN_BYTES: usize = 1 << 22;
const MAX_CHECK_THREADS: usize = 8;

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

struct Source<'a> {
    file: &'a File,
    path: &'a Path,
    sections: [Range<usize>; SECTIONS],
}

impl Source<'_> {
    fn broken(&self, reason: &'static str) -> Error {
        Error::Format {
            file: self.path.to_path_buf(),
            reason,
        }
    }

    fn read(&self, section: usize, from: usize, buffer: &mut [u8]) -> Result<(), Error> {
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
    cells: Section<[u32; 2]>,
    token_bytes: usize,
    token_count: usize,
    left_size: usize,
    right_size: usize,
    category_count: usize,
    hot: bool,
    integer_templates: bool,
    ascii: [u16; 128],
    bos_feature: Range<usize>,
    unknown_feature: Option<Range<usize>>,
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
        if header[..8] != *MAGIC || u32_at(&header, 8) != VERSION {
            return Err(broken("the magic number or the version is not valid"));
        }
        let flags = u32_at(&header, 12);
        if flags & !(FLAG_WIDE_PARAMS | FLAG_HOT_MATRIX | FLAG_INT_TEMPLATES) != 0 {
            return Err(broken("the flags are not valid"));
        }
        if flags & FLAG_INT_TEMPLATES != 0 && flags & FLAG_HOT_MATRIX == 0 {
            return Err(broken("integer templates require a hot matrix"));
        }
        let token_bytes = if flags & FLAG_WIDE_PARAMS != 0 { 8 } else { 6 };
        let code_count = u32_at(&header, 16) as usize;
        let token_count = u32_at(&header, 20) as usize;
        let param_count = u32_at(&header, 24) as usize;
        let category_count = u32_at(&header, 28) as usize;
        if u32_at(&header, 32) != 0 || u32_at(&header, 36) != 0 {
            return Err(broken("a reserved field is not zero"));
        }
        let mut sections: [Range<usize>; SECTIONS] = Default::default();
        let mut end = HEADER_BYTES as u64;
        for (number, section) in sections.iter_mut().enumerate() {
            let at = 40 + 16 * number;
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
        let length = |section: usize| sections[section].len();
        if length(CHAR_INDEX) != BLOCK_COUNT * 2
            || length(CHAR_BLOCKS) == 0
            || length(CHAR_BLOCKS) % (BLOCK_CODES * 2) != 0
            || length(CELLS) == 0
            || length(CELLS) % 8 != 0
            || length(CELLS) / 8 > MAX_UNITS
            || length(TOKENS) != token_count * token_bytes
            || length(PARAMS) != param_count * PARAM_BYTES
            || length(UNKNOWN_STARTS) != (category_count + 1) * 4
            || length(UNKNOWN_TOKENS) % token_bytes != 0
            || code_count == 0
            || code_count > 1 << 16
            || token_count > MAX_UNITS
            || length(FEATURES) > u32::MAX as usize
            || (token_bytes == 6 && param_count > 1 << 16)
        {
            return Err(broken("the size of a section does not match the header"));
        }
        let unknown_count = length(UNKNOWN_TOKENS) / token_bytes;

        let mut matrix_header = [0u8; 4];
        if length(MATRIX) < 4 {
            return Err(broken("the matrix is shorter than its header"));
        }
        source.read(MATRIX, 0, &mut matrix_header)?;
        let left_size = usize::from(u16_at(&matrix_header, 0));
        let right_size = usize::from(u16_at(&matrix_header, 2));
        let hot = flags & FLAG_HOT_MATRIX != 0;
        let integer_templates = flags & FLAG_INT_TEMPLATES != 0;
        let (header_bytes, hot_cells) = if hot {
            let mut hot_header = [0u8; 4];
            source.read(MATRIX, 4, &mut hot_header)?;
            let hot_left = usize::from(u16_at(&hot_header, 0));
            let hot_right = usize::from(u16_at(&hot_header, 2));
            if hot_left == 0 || hot_right == 0 || hot_left > left_size || hot_right > right_size {
                return Err(broken("the hot block lies outside of the matrix"));
            }
            (8, hot_left * hot_right)
        } else {
            (4, left_size * right_size)
        };
        if left_size == 0 || right_size == 0 || length(MATRIX) != header_bytes + hot_cells * 2 {
            return Err(broken("the matrix size does not match its header"));
        }
        if hot && integer_templates {
            let mut templates_header = [0u8; 24];
            if length(TEMPLATES) < templates_header.len() {
                return Err(broken("the integer templates are empty"));
            }
            source.read(TEMPLATES, 0, &mut templates_header)?;
            if templates_header[..4] != *b"MXIT"
                || u32_at(&templates_header, 4) != 1
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

        let features_length = length(FEATURES);
        let starts = check_features(&source, features_length)?;

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

        let valid_tokens = |_: usize, part: &[u8]| {
            part.chunks_exact(token_bytes).all(|token| {
                let (param, feature) = if token_bytes == 8 {
                    (u32_at(token, 0) as usize, u32_at(token, 4) as usize)
                } else {
                    (usize::from(u16_at(token, 0)), u32_at(token, 2) as usize)
                };
                param < param_count
                    && feature < features_length
                    && starts.binary_search(&(feature as u32)).is_ok()
            })
        };
        let reason = "a token has a parameter set or a feature that does not exist";
        source.chunks(TOKENS, token_bytes, valid_tokens, reason)?;
        source.chunks(UNKNOWN_TOKENS, token_bytes, valid_tokens, reason)?;
        drop(starts);
        let unknown_starts = source.read_all(UNKNOWN_STARTS)?;
        let unknown_starts: Vec<u32> = unknown_starts
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&word| u32::from_le_bytes(word))
            .collect();
        if unknown_starts[0] != 0
            || unknown_starts.windows(2).any(|pair| pair[0] > pair[1])
            || unknown_starts[category_count] as usize != unknown_count
        {
            return Err(broken("the unknown tokens of the categories do not add up"));
        }

        check_cells(&source, token_count)?;

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

        let map = Arc::new(map_file(path)?);
        if map.len() as u64 != file_length {
            return Err(broken("the file changed while it was opened"));
        }
        let view_error = || broken("a section is misaligned");
        let char_index = Section::new(&map, sections[CHAR_INDEX].clone()).ok_or_else(view_error)?;
        let char_blocks =
            Section::new(&map, sections[CHAR_BLOCKS].clone()).ok_or_else(view_error)?;
        let cells = Section::new(&map, sections[CELLS].clone()).ok_or_else(view_error)?;
        if hot && integer_templates {
            let template = MappedBytes::from_arc(Arc::clone(&map), sections[TEMPLATES].clone())
                .expect("the template section is inside the mapping");
            MappedIntTemplates::from_mapped(template, path)?;
        }
        let dicrc_start = sections[DICRC].start + 8;
        let mut dictionary = Self {
            map,
            char_index,
            char_blocks,
            cells,
            token_bytes,
            token_count,
            left_size,
            right_size,
            category_count,
            hot: flags & FLAG_HOT_MATRIX != 0,
            integer_templates,
            bos_feature: dicrc_start..dicrc_start + bos_length,
            unknown_feature: (unknown_length > 0)
                .then(|| dicrc_start + bos_length..dicrc_start + bos_length + unknown_length),
            ascii: [0; 128],
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
    pub fn code_of(&self, char: char) -> u16 {
        let scalar = char as usize;
        let index = self.char_index.view(&self.map);
        let blocks = self.char_blocks.view(&self.map);
        let block = usize::from(index[scalar / BLOCK_CODES]);
        blocks[block * BLOCK_CODES + scalar % BLOCK_CODES]
    }

    pub fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<Hit>) {
        hits.clear();
        self.walk_bytes(key, |hit| hits.push(hit));
    }

    #[inline(always)]
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
        self.walk_bytes(key, |hit| found = Some(hit));
        found.filter(|hit| hit.length == key.len()).map(Hit::tokens)
    }

    #[inline(always)]
    fn walk_bytes(&self, key: &[u8], mut found: impl FnMut(Hit)) {
        let mut cursor = Cursor::new(self.cells.view(&self.map));
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
    fn tokens(&self) -> &[u8] {
        &self.map[self.sections[TOKENS].clone()]
    }

    fn token_at(&self, bytes: &[u8], index: usize) -> TokenRecord {
        let width = self.token_bytes;
        let token = &bytes[index * width..(index + 1) * width];
        let (param, feature) = if width == 8 {
            (u32_at(token, 0) as usize, u32_at(token, 4))
        } else {
            (usize::from(u16_at(token, 0)), u32_at(token, 2))
        };
        let params = &self.map[self.sections[PARAMS].clone()];
        let param = &params[param * PARAM_BYTES..(param + 1) * PARAM_BYTES];
        TokenRecord {
            left_id: u16_at(param, 0),
            right_id: u16_at(param, 2),
            pos_id: u16_at(param, 4),
            word_cost: u16_at(param, 6).cast_signed(),
            feature,
        }
    }

    #[inline]
    pub fn token(&self, index: usize) -> TokenRecord {
        self.token_at(self.tokens(), index)
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
        range.map(move |index| self.token_at(bytes, index))
    }

    pub fn feature(&self, offset: u32) -> &str {
        let area = &self.map[self.sections[FEATURES].clone()];
        let area = unsafe { str::from_utf8_unchecked(area) };
        let rest = area
            .get(offset as usize..)
            .expect("feature offset is outside the feature area");
        let length = rest
            .find('\0')
            .expect("open checked that the features end with a NUL byte");
        &rest[..length]
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

struct Cursor<'a> {
    cells: &'a [[u32; 2]],
    node: u32,
    base: u32,
}

impl<'a> Cursor<'a> {
    #[inline(always)]
    fn new(cells: &'a [[u32; 2]]) -> Self {
        Self {
            cells,
            node: 0,
            base: cells[0][0],
        }
    }

    #[inline(always)]
    fn step(&mut self, code: u16) -> Step {
        if code == 0 {
            return Step::Stop;
        }
        let index = (self.base as usize).wrapping_add(usize::from(code));
        let Some(&[next_base, check]) = self.cells.get(index) else {
            return Step::Stop;
        };
        if check & PARENT_MASK != self.node {
            return Step::Stop;
        }
        self.node = index as u32;
        self.base = next_base;
        if check & TERMINAL_FLAG == 0 {
            Step::Inner
        } else if check & INLINE_FLAG != 0 {
            Step::Key(next_base, (check >> COUNT_SHIFT) & INLINE_COUNT_MASK)
        } else if let Some(&[data, _]) = self.cells.get(next_base as usize) {
            Step::Key(data & PARENT_MASK, data >> COUNT_SHIFT)
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

fn check_features(source: &Source, length: usize) -> Result<Vec<u32>, Error> {
    let threads = if length < PARALLEL_MIN_BYTES {
        1
    } else {
        thread::available_parallelism().map_or(1, |count| count.get().min(MAX_CHECK_THREADS))
    };
    let mut bounds = vec![0];
    for part in 1..threads {
        bounds.push(next_boundary(source, length / threads * part, length)?);
    }
    bounds.push(length);
    let results: Vec<Result<Vec<u32>, Error>> = thread::scope(|scope| {
        let handles: Vec<_> = bounds
            .windows(2)
            .filter(|pair| pair[0] < pair[1])
            .map(|pair| {
                let (begin, end) = (pair[0], pair[1]);
                scope.spawn(move || check_feature_range(source, begin..end, length))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a check thread panicked"))
            .collect()
    });
    let mut starts = Vec::new();
    if length > 0 {
        starts.push(0);
    }
    for result in results {
        starts.extend(result?);
    }
    Ok(starts)
}

fn next_boundary(source: &Source, from: usize, length: usize) -> Result<usize, Error> {
    let mut buffer = vec![0u8; 4096];
    let mut at = from.saturating_sub(1);
    while at < length {
        let part = buffer.len().min(length - at);
        source.read(FEATURES, at, &mut buffer[..part])?;
        if let Some(found) = buffer[..part].iter().position(|&byte| byte == 0) {
            return Ok(at + found + 1);
        }
        at += part;
    }
    Ok(length)
}

fn check_feature_range(
    source: &Source,
    range: Range<usize>,
    length: usize,
) -> Result<Vec<u32>, Error> {
    let mut buffer = vec![0u8; CHUNK.min(range.len())];
    let mut starts = Vec::new();
    let mut carry = 0;
    let mut done = range.start;
    while done < range.end {
        let part = CHUNK.min(range.end - done);
        if buffer.len() < carry + part {
            buffer.resize(carry + part, 0);
        }
        source.read(FEATURES, done, &mut buffer[carry..carry + part])?;
        let filled = carry + part;
        let base = done - carry;
        let mut cut = 0;
        for (at, &byte) in buffer[..filled].iter().enumerate() {
            if byte == 0 {
                cut = at + 1;
                let start = base + cut;
                if start < length && u32::try_from(start).is_ok() {
                    starts.push(start as u32);
                }
            }
        }
        if str::from_utf8(&buffer[..cut]).is_err() {
            return Err(source.broken("the features are not valid UTF-8"));
        }
        buffer.copy_within(cut..filled, 0);
        carry = filled - cut;
        done += part;
    }
    if carry != 0 {
        return Err(source.broken("the features do not end with a NUL byte"));
    }
    Ok(starts)
}

fn check_cells(source: &Source, token_count: usize) -> Result<(), Error> {
    let cell_count = source.sections[CELLS].len() / 8;
    let reason = "a double array unit points outside the token table";
    let valid_range =
        |first: u32, count: u32| count > 0 && first as usize + count as usize <= token_count;
    let mut data = vec![0u64; cell_count.div_ceil(64)];
    let mut read_error = None;
    let first_pass = source.chunks(
        CELLS,
        8,
        |first, part| {
            for (offset, unit) in part.as_chunks::<8>().0.iter().enumerate() {
                let index = first + offset;
                let base = u32_at(unit, 0);
                let check = u32_at(unit, 4);
                if check == UNUSED_CHECK || check & TERMINAL_FLAG == 0 || check & INLINE_FLAG != 0 {
                    continue;
                }
                let base = base as usize;
                if base >= cell_count || base == index {
                    return false;
                }
                let mut target = [0u8; 8];
                if let Err(error) = source.read(CELLS, base * 8, &mut target) {
                    read_error = Some(error);
                    return false;
                }
                let tokens = u32_at(&target, 0);
                let data_check = u32_at(&target, 4);
                if data_check != index as u32
                    || !valid_range(tokens & PARENT_MASK, tokens >> COUNT_SHIFT)
                {
                    return false;
                }
                data[base / 64] |= 1 << (base % 64);
            }
            true
        },
        reason,
    );
    if let Some(error) = read_error {
        return Err(error);
    }
    first_pass?;
    source.chunks(
        CELLS,
        8,
        |first, part| {
            for (offset, unit) in part.as_chunks::<8>().0.iter().enumerate() {
                let index = first + offset;
                if data[index / 64] >> (index % 64) & 1 != 0 {
                    continue;
                }
                let base = u32_at(unit, 0);
                let check = u32_at(unit, 4);
                if check == UNUSED_CHECK {
                    continue;
                }
                let inline = check & INLINE_FLAG != 0;
                let terminal = check & TERMINAL_FLAG != 0;
                if inline && !terminal
                    || inline && !valid_range(base, (check >> COUNT_SHIFT) & INLINE_COUNT_MASK)
                {
                    return false;
                }
            }
            true
        },
        reason,
    )
}
