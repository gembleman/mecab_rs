use std::path::Path;

use crate::char_layout::{
    CATEGORY_NAME_BYTES, DEFAULT_TYPE_MASK, DEFAULT_TYPE_SHIFT, FLAG_MASK, GROUP_SHIFT,
    INVOKE_SHIFT, LENGTH_MASK, LENGTH_SHIFT, TYPE_MASK,
};
use crate::char_table::{
    BMP_LEN, CHAR_TABLE_HEADER_BYTES, CODE_POINTS, EXTENDER_FLAG, MAGIC, MAX_PALETTE,
    PALETTE_ENTRY_BYTES, VERSION,
};
use crate::dictionary::map_file;
use crate::error::Error;
use crate::mapped_bytes::MappedBytes;
use crate::utf8::{
    CONTINUATION_PAYLOAD, FOUR_BYTE_LEAD, FOUR_BYTE_PAYLOAD, THREE_BYTE_PAYLOAD, TWO_BYTE_PAYLOAD,
    is_continuation, is_four_byte_lead, is_three_byte_lead, is_two_byte_lead,
};
use crate::wire::u32_at;

const SPACE_CODE: u32 = 0x20;

#[derive(Clone, Copy, Default)]
pub(crate) struct CharInfo(u32);

impl CharInfo {
    fn type_mask(self) -> u32 {
        self.0 & TYPE_MASK
    }

    pub(crate) fn default_type(self) -> usize {
        ((self.0 >> DEFAULT_TYPE_SHIFT) & DEFAULT_TYPE_MASK) as usize
    }

    pub(crate) fn length(self) -> usize {
        ((self.0 >> LENGTH_SHIFT) & LENGTH_MASK) as usize
    }

    pub(crate) fn group(self) -> bool {
        (self.0 >> GROUP_SHIFT) & FLAG_MASK == FLAG_MASK
    }

    pub(crate) fn invoke(self) -> bool {
        (self.0 >> INVOKE_SHIFT) & FLAG_MASK == FLAG_MASK
    }

    pub(crate) fn is_kind_of(self, other: CharInfo) -> bool {
        self.type_mask() & other.type_mask() != 0
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Scan {
    pub(crate) info: CharInfo,
    pub(crate) width: usize,
    pub(crate) count: usize,
}

#[derive(Clone, Copy, Default)]
struct Class {
    info: CharInfo,
    extender: bool,
}

#[derive(Clone, Copy, Default)]
struct Entry {
    info: CharInfo,
    extender: bool,
    single: u8,
    trie_code: u16,
    width: u32,
    chain_end: u32,
    chain_count: u32,
    chain_last: u32,
}

const ENTRY_BYTES: usize = 24;
const _: () = assert!(std::mem::size_of::<Entry>() == ENTRY_BYTES);

#[derive(Default)]
pub(crate) struct SentenceChars {
    entries: Vec<Entry>,
    length: usize,
}

impl SentenceChars {
    pub(crate) fn fill(
        &mut self,
        chars: &CharProperty,
        text: &[u8],
        trie_code_of: impl Fn(u32) -> u16,
    ) {
        let length = text.len();
        let table = chars.table();
        assert!(u32::try_from(length).is_ok());
        self.length = length;
        if self.entries.len() <= length {
            self.entries.resize(length + 1, Entry::default());
        }
        self.entries[length] = Entry {
            info: chars.class_of_table_code(table, 0).info,
            single: 1,
            width: 1,
            ..Entry::default()
        };
        debug_assert!(std::str::from_utf8(text).is_ok());
        for position in (0..length).rev() {
            if is_continuation(text[position]) {
                #[cfg(debug_assertions)]
                {
                    self.entries[position] = Entry::default();
                }
                continue;
            }
            let (code, single) = decode_utf8(text, position, length - position);
            let class = chars.class_of_table_code(table, code);
            let following = *self.entry(position + single);
            let width = single as u32
                + if following.extender {
                    following.width
                } else {
                    0
                };
            let next = position + width as usize;
            let after = *self.entry(next);
            let (chain_end, chain_count, chain_last) =
                if next < length && class.info.is_kind_of(after.info) {
                    (after.chain_end, after.chain_count + 1, after.chain_last)
                } else {
                    (next as u32, 1, position as u32)
                };
            self.entries[position] = Entry {
                info: class.info,
                extender: class.extender,
                single: single as u8,
                trie_code: trie_code_of(code),
                width,
                chain_end,
                chain_count,
                chain_last,
            };
        }
    }

    #[inline]
    fn entry(&self, position: usize) -> &Entry {
        let entry = &self.entries[position];
        debug_assert!(entry.single != 0, "entry read at a continuation byte");
        entry
    }

    #[inline]
    pub(crate) fn trie_step(&self, position: usize) -> (u16, usize) {
        let entry = self.entry(position);
        (entry.trie_code, usize::from(entry.single))
    }

    pub(crate) fn len(&self) -> usize {
        self.length
    }

    pub(crate) fn char_info(&self, position: usize) -> (CharInfo, usize) {
        let entry = self.entry(position);
        (entry.info, entry.width as usize)
    }

    pub(crate) fn seek_other_type(&self, begin: usize, start: CharInfo) -> (usize, Scan) {
        let length = self.length;
        if begin >= length {
            return (begin, Scan::default());
        }
        let entry = self.entry(begin);
        if !start.is_kind_of(entry.info) {
            let scan = Scan {
                info: entry.info,
                width: entry.width as usize,
                count: 0,
            };
            return (begin, scan);
        }
        let chain_end = entry.chain_end as usize;
        let described = self.entry(if chain_end < length {
            chain_end
        } else {
            entry.chain_last as usize
        });
        let scan = Scan {
            info: described.info,
            width: described.width as usize,
            count: entry.chain_count as usize,
        };
        (chain_end, scan)
    }

    pub(crate) fn skip_space(&self, begin: usize, space: CharInfo) -> (usize, Scan) {
        let length = self.length;
        let mut position = begin;
        let mut current = space;
        let mut scan = Scan::default();
        while position < length {
            let entry = self.entry(position);
            scan.info = entry.info;
            scan.width = entry.width as usize;
            if !current.is_kind_of(entry.info) {
                break;
            }
            position += usize::from(entry.single);
            scan.count += 1;
            current = entry.info;
        }
        (position, scan)
    }
}

pub struct CharProperty {
    map: MappedBytes,
    category_count: usize,
    palette: Vec<Class>,
    table_start: usize,
    space: CharInfo,
}

impl CharProperty {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Err(Error::MissingCharTable {
                path: path.to_path_buf(),
            });
        }
        let map = MappedBytes::whole(map_file(path)?);
        Self::from_mapped(map, path)
    }

    pub(crate) fn from_mapped(map: MappedBytes, path: &Path) -> Result<Self, Error> {
        let broken = |reason| Error::format(path, reason);
        if map.len() < CHAR_TABLE_HEADER_BYTES || map[..4] != *MAGIC || u32_at(&map, 4) != VERSION {
            return Err(broken("the file is not a character table of this version"));
        }
        let category_count = u32_at(&map, 8) as usize;
        let palette_len = u32_at(&map, 12) as usize;
        if category_count == 0
            || category_count > DEFAULT_TYPE_MASK as usize
            || palette_len == 0
            || palette_len > MAX_PALETTE
        {
            return Err(broken("the category or palette count is out of range"));
        }
        let palette_start = CHAR_TABLE_HEADER_BYTES + CATEGORY_NAME_BYTES * category_count;
        let table_start = palette_start + PALETTE_ENTRY_BYTES * palette_len;
        if table_start + CODE_POINTS != map.len() {
            return Err(broken("the file size does not match the header"));
        }
        let mut palette = Vec::with_capacity(palette_len);
        for index in 0..palette_len {
            let at = palette_start + PALETTE_ENTRY_BYTES * index;
            let (info, flags) = (u32_at(&map, at), u32_at(&map, at + 4));
            let info = CharInfo(info);
            if info.default_type() >= category_count || flags & !EXTENDER_FLAG != 0 {
                return Err(broken("a palette entry is not valid"));
            }
            palette.push(Class {
                info,
                extender: flags & EXTENDER_FLAG != 0,
            });
        }
        let table = map
            .slice(table_start..map.len())
            .expect("the character table size was checked above");
        if table.iter().any(|&index| usize::from(index) >= palette_len) {
            return Err(broken("a character refers to a missing palette entry"));
        }
        if table[..BMP_LEN]
            .iter()
            .any(|&index| palette[usize::from(index)].extender)
        {
            return Err(broken("a code point of the BMP is an extender"));
        }
        let space = palette[usize::from(table[SPACE_CODE as usize])].info;
        Ok(Self {
            map,
            category_count,
            palette,
            table_start,
            space,
        })
    }

    pub(crate) fn open_bytes(path: &Path, map: MappedBytes) -> Result<Self, Error> {
        Self::from_mapped(map, path)
    }

    pub fn category_count(&self) -> usize {
        self.category_count
    }

    pub fn name(&self, index: usize) -> &[u8] {
        let start = CHAR_TABLE_HEADER_BYTES + CATEGORY_NAME_BYTES * index;
        let bytes = self
            .map
            .slice(start..start + CATEGORY_NAME_BYTES)
            .expect("the category name is inside the validated table");
        let length = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(CATEGORY_NAME_BYTES);
        &bytes[..length]
    }

    fn class_of_code(&self, code: u32) -> Class {
        let table = self.table();
        self.class_of_table_code(table, code)
    }

    #[inline]
    fn class_of_table_code(&self, table: &[u8], code: u32) -> Class {
        let index = table[code as usize];
        self.palette[usize::from(index)]
    }

    #[inline]
    fn table(&self) -> &[u8] {
        self.map
            .slice(self.table_start..self.map.len())
            .expect("the character table is inside the validated map")
    }

    #[inline]
    pub(crate) fn space(&self) -> CharInfo {
        self.space
    }

    fn code_class(&self, text: &[u8], begin: usize, end: usize) -> (Class, usize) {
        let (code, width) = decode_utf8(text, begin, end - begin);
        (self.class_of_code(code), width)
    }

    fn extenders_width(&self, text: &[u8], begin: usize, end: usize) -> usize {
        let mut position = begin;
        while position < end && text[position] >= FOUR_BYTE_LEAD {
            let (class, width) = self.code_class(text, position, end);
            if !class.extender {
                break;
            }
            position += width;
        }
        position - begin
    }

    pub(crate) fn extended_end(&self, text: &[u8], position: usize, limit: usize) -> usize {
        if position < limit && text[position] >= FOUR_BYTE_LEAD {
            position + self.extenders_width(text, position, limit)
        } else {
            position
        }
    }

    pub(crate) fn char_info(&self, text: &[u8], begin: usize, end: usize) -> (CharInfo, usize) {
        let (class, width) = self.code_class(text, begin, end);
        let extenders = self.extenders_width(text, begin + width, end);
        (class.info, width + extenders)
    }

    pub(crate) fn seek_other_type(
        &self,
        text: &[u8],
        begin: usize,
        end: usize,
        start: CharInfo,
    ) -> (usize, Scan) {
        self.seek(text, begin, end, start, true)
    }

    pub(crate) fn skip_space(&self, text: &[u8], begin: usize, end: usize) -> (usize, Scan) {
        self.seek(text, begin, end, self.space(), false)
    }

    fn seek(
        &self,
        text: &[u8],
        begin: usize,
        end: usize,
        start: CharInfo,
        absorb: bool,
    ) -> (usize, Scan) {
        let mut position = begin;
        let mut current = start;
        let mut scan = Scan::default();
        while position < end {
            let (class, single) = self.code_class(text, position, end);
            let width = single + self.extenders_width(text, position + single, end);
            scan.info = class.info;
            scan.width = width;
            if !current.is_kind_of(class.info) {
                break;
            }
            position += if absorb { width } else { single };
            scan.count += 1;
            current = class.info;
        }
        (position, scan)
    }
}

#[inline]
fn decode_utf8(text: &[u8], begin: usize, available: usize) -> (u32, usize) {
    let byte = |offset: usize| text.get(begin + offset).copied().unwrap_or(0);
    let first = byte(0);
    if first < 0x80 {
        (u32::from(first), 1)
    } else if available >= 2 && is_two_byte_lead(first) {
        (
            u32::from(first & TWO_BYTE_PAYLOAD) << 6 | u32::from(byte(1) & CONTINUATION_PAYLOAD),
            2,
        )
    } else if available >= 3 && is_three_byte_lead(first) {
        let code = u32::from(first & THREE_BYTE_PAYLOAD) << 12
            | u32::from(byte(1) & CONTINUATION_PAYLOAD) << 6
            | u32::from(byte(2) & CONTINUATION_PAYLOAD);
        (code, 3)
    } else if available >= 4 && is_four_byte_lead(first) {
        let code = u32::from(first & FOUR_BYTE_PAYLOAD) << 18
            | u32::from(byte(1) & CONTINUATION_PAYLOAD) << 12
            | u32::from(byte(2) & CONTINUATION_PAYLOAD) << 6
            | u32::from(byte(3) & CONTINUATION_PAYLOAD);
        (code, 4)
    } else {
        (0, 1)
    }
}
