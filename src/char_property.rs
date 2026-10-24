use std::path::Path;

use memmap2::Mmap;

use crate::char_table::{
    BMP_LEN, CODE_POINTS, DEFAULT_TYPE_SHIFT, EXTENDER_FLAG, GROUP_SHIFT, HEADER_BYTES,
    INVOKE_SHIFT, LENGTH_SHIFT, MAGIC, MAX_PALETTE, NAME_BYTES, PALETTE_ENTRY_BYTES, TYPE_BITS,
    VERSION,
};
use crate::dictionary::map_file;
use crate::error::Error;

const SPACE_CODE: u32 = 0x20;
const FOUR_BYTE_LEAD: u8 = 0xf0;

#[derive(Clone, Copy, Default)]
pub(crate) struct CharInfo(u32);

impl CharInfo {
    fn type_mask(self) -> u32 {
        self.0 & ((1 << TYPE_BITS) - 1)
    }

    pub(crate) fn default_type(self) -> usize {
        ((self.0 >> DEFAULT_TYPE_SHIFT) & 0xff) as usize
    }

    pub(crate) fn length(self) -> usize {
        ((self.0 >> LENGTH_SHIFT) & 0xf) as usize
    }

    pub(crate) fn group(self) -> bool {
        (self.0 >> GROUP_SHIFT) & 1 == 1
    }

    pub(crate) fn invoke(self) -> bool {
        (self.0 >> INVOKE_SHIFT) & 1 == 1
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
    single: usize,
    width: usize,
    chain_end: usize,
    chain_count: usize,
    chain_last: usize,
}

#[derive(Default)]
pub(crate) struct SentenceChars {
    entries: Vec<Entry>,
}

impl SentenceChars {
    pub(crate) fn fill(&mut self, chars: &CharProperty, text: &[u8]) {
        let length = text.len();
        self.entries.clear();
        self.entries.resize(length + 1, Entry::default());
        self.entries[length] = Entry {
            info: chars.class_of_code(0).info,
            single: 1,
            width: 1,
            ..Entry::default()
        };
        for position in (0..length).rev() {
            let (code, single) = decode_utf8(text, position, length - position);
            let class = chars.class_of_code(code);
            let following = self.entries[position + single];
            let width = single
                + if following.extender {
                    following.width
                } else {
                    0
                };
            let next = position + width;
            let after = self.entries[next];
            let (chain_end, chain_count, chain_last) =
                if next < length && class.info.is_kind_of(after.info) {
                    (after.chain_end, after.chain_count + 1, after.chain_last)
                } else {
                    (next, 1, position)
                };
            self.entries[position] = Entry {
                info: class.info,
                extender: class.extender,
                single,
                width,
                chain_end,
                chain_count,
                chain_last,
            };
        }
    }

    pub(crate) fn char_info(&self, position: usize) -> (CharInfo, usize) {
        let entry = &self.entries[position];
        (entry.info, entry.width)
    }

    pub(crate) fn seek_other_type(&self, begin: usize, start: CharInfo) -> (usize, Scan) {
        let length = self.entries.len() - 1;
        if begin >= length {
            return (begin, Scan::default());
        }
        let entry = &self.entries[begin];
        if !start.is_kind_of(entry.info) {
            let scan = Scan {
                info: entry.info,
                width: entry.width,
                count: 0,
            };
            return (begin, scan);
        }
        let described = &self.entries[if entry.chain_end < length {
            entry.chain_end
        } else {
            entry.chain_last
        }];
        let scan = Scan {
            info: described.info,
            width: described.width,
            count: entry.chain_count,
        };
        (entry.chain_end, scan)
    }

    pub(crate) fn skip_space(&self, begin: usize, space: CharInfo) -> (usize, Scan) {
        let length = self.entries.len() - 1;
        let mut position = begin;
        let mut current = space;
        let mut scan = Scan::default();
        while position < length {
            let entry = &self.entries[position];
            scan.info = entry.info;
            scan.width = entry.width;
            if !current.is_kind_of(entry.info) {
                break;
            }
            position += entry.single;
            scan.count += 1;
            current = entry.info;
        }
        (position, scan)
    }
}

pub(crate) struct CharProperty {
    map: Mmap,
    category_count: usize,
    palette: Vec<Class>,
    table_start: usize,
}

impl CharProperty {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Err(Error::MissingCharTable {
                path: path.to_path_buf(),
            });
        }
        let map = map_file(path)?;
        Self::from_map(map, path)
    }

    fn from_map(map: Mmap, path: &Path) -> Result<Self, Error> {
        let broken = |reason| Error::Format {
            file: path.to_path_buf(),
            reason,
        };
        let word = |at: usize| u32::from_le_bytes(*map[at..].first_chunk().expect("in range"));
        if map.len() < HEADER_BYTES || map[..4] != *MAGIC || word(4) != VERSION {
            return Err(broken("the file is not a character table of this version"));
        }
        let category_count = word(8) as usize;
        let palette_len = word(12) as usize;
        if category_count == 0
            || category_count > 0xff
            || palette_len == 0
            || palette_len > MAX_PALETTE
        {
            return Err(broken("the category or palette count is out of range"));
        }
        let palette_start = HEADER_BYTES + NAME_BYTES * category_count;
        let table_start = palette_start + PALETTE_ENTRY_BYTES * palette_len;
        if table_start + CODE_POINTS != map.len() {
            return Err(broken("the file size does not match the header"));
        }
        let mut palette = Vec::with_capacity(palette_len);
        for index in 0..palette_len {
            let at = palette_start + PALETTE_ENTRY_BYTES * index;
            let (info, flags) = (word(at), word(at + 4));
            let info = CharInfo(info);
            if info.default_type() >= category_count || flags & !EXTENDER_FLAG != 0 {
                return Err(broken("a palette entry is not valid"));
            }
            palette.push(Class {
                info,
                extender: flags & EXTENDER_FLAG != 0,
            });
        }
        let table = &map[table_start..];
        if table.iter().any(|&index| usize::from(index) >= palette_len) {
            return Err(broken("a character refers to a missing palette entry"));
        }
        if table[..BMP_LEN]
            .iter()
            .any(|&index| palette[usize::from(index)].extender)
        {
            return Err(broken("a code point of the BMP is an extender"));
        }
        Ok(Self {
            map,
            category_count,
            palette,
            table_start,
        })
    }

    pub(crate) fn category_count(&self) -> usize {
        self.category_count
    }

    pub(crate) fn name(&self, index: usize) -> &[u8] {
        let start = HEADER_BYTES + NAME_BYTES * index;
        let bytes = &self.map[start..start + NAME_BYTES];
        let length = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(NAME_BYTES);
        &bytes[..length]
    }

    fn class_of_code(&self, code: u32) -> Class {
        self.palette[usize::from(self.map[self.table_start + code as usize])]
    }

    pub(crate) fn space(&self) -> CharInfo {
        self.class_of_code(SPACE_CODE).info
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

fn decode_utf8(text: &[u8], begin: usize, available: usize) -> (u32, usize) {
    let byte = |offset: usize| text.get(begin + offset).copied().unwrap_or(0);
    let first = byte(0);
    if first < 0x80 {
        (u32::from(first), 1)
    } else if available >= 2 && first & 0xe0 == 0xc0 {
        (u32::from(first & 0x1f) << 6 | u32::from(byte(1) & 0x3f), 2)
    } else if available >= 3 && first & 0xf0 == 0xe0 {
        let code = u32::from(first & 0x0f) << 12
            | u32::from(byte(1) & 0x3f) << 6
            | u32::from(byte(2) & 0x3f);
        (code, 3)
    } else if available >= 4 && first & 0xf8 == 0xf0 {
        let code = u32::from(first & 0x07) << 18
            | u32::from(byte(1) & 0x3f) << 12
            | u32::from(byte(2) & 0x3f) << 6
            | u32::from(byte(3) & 0x3f);
        (code, 4)
    } else {
        (0, 1)
    }
}
