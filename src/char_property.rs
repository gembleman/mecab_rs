use std::path::Path;

use memmap2::Mmap;

use crate::dictionary::map_file;
use crate::error::Error;

const NAME_BYTES: usize = 32;
const TABLE_ENTRIES: usize = 0xffff;
const ENTRY_BYTES: usize = 4;
const TYPE_BITS: u32 = 18;
const DEFAULT_TYPE_BITS: u32 = 8;
const LENGTH_BITS: u32 = 4;
const SPACE_CODE: u16 = 0x20;

#[derive(Clone, Copy, Default)]
pub(crate) struct CharInfo(u32);

impl CharInfo {
    fn type_mask(self) -> u32 {
        self.0 & ((1 << TYPE_BITS) - 1)
    }

    pub(crate) fn default_type(self) -> usize {
        ((self.0 >> TYPE_BITS) & ((1 << DEFAULT_TYPE_BITS) - 1)) as usize
    }

    pub(crate) fn length(self) -> usize {
        ((self.0 >> (TYPE_BITS + DEFAULT_TYPE_BITS)) & ((1 << LENGTH_BITS) - 1)) as usize
    }

    pub(crate) fn group(self) -> bool {
        (self.0 >> (TYPE_BITS + DEFAULT_TYPE_BITS + LENGTH_BITS)) & 1 == 1
    }

    pub(crate) fn invoke(self) -> bool {
        (self.0 >> (TYPE_BITS + DEFAULT_TYPE_BITS + LENGTH_BITS + 1)) & 1 == 1
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

pub(crate) struct CharProperty {
    map: Mmap,
    category_count: usize,
    table_start: usize,
}

impl CharProperty {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let map = map_file(path)?;
        let broken = |reason| Error::Format { file: path.to_path_buf(), reason };
        if map.len() < 4 {
            return Err(broken("the file is shorter than its header"));
        }
        let category_count = u32::from_le_bytes([map[0], map[1], map[2], map[3]]) as usize;
        let table_start = 4 + NAME_BYTES * category_count;
        if table_start + ENTRY_BYTES * TABLE_ENTRIES != map.len() {
            return Err(broken("the file size does not match the header"));
        }
        Ok(Self { map, category_count, table_start })
    }

    pub(crate) fn category_count(&self) -> usize {
        self.category_count
    }

    pub(crate) fn name(&self, index: usize) -> &[u8] {
        let start = 4 + NAME_BYTES * index;
        let bytes = &self.map[start..start + NAME_BYTES];
        let length = bytes.iter().position(|&byte| byte == 0).unwrap_or(NAME_BYTES);
        &bytes[..length]
    }

    fn info_of_code(&self, code: u16) -> CharInfo {
        let index = usize::from(code);
        if index >= TABLE_ENTRIES {
            return CharInfo::default();
        }
        let at = self.table_start + index * ENTRY_BYTES;
        CharInfo(u32::from_le_bytes([
            self.map[at],
            self.map[at + 1],
            self.map[at + 2],
            self.map[at + 3],
        ]))
    }

    pub(crate) fn space(&self) -> CharInfo {
        self.info_of_code(SPACE_CODE)
    }

    pub(crate) fn char_info(&self, text: &[u8], begin: usize, end: usize) -> (CharInfo, usize) {
        let (code, width) = decode_utf8(text, begin, end.wrapping_sub(begin));
        (self.info_of_code(code), width)
    }

    pub(crate) fn seek_other_type(
        &self,
        text: &[u8],
        begin: usize,
        end: usize,
        start: CharInfo,
        scan: &mut Scan,
    ) -> usize {
        let mut position = begin;
        let mut current = start;
        scan.count = 0;
        while position < end {
            let (info, width) = self.char_info(text, position, end);
            scan.info = info;
            scan.width = width;
            if !current.is_kind_of(info) {
                break;
            }
            position += width;
            scan.count += 1;
            current = info;
        }
        position
    }
}

fn decode_utf8(text: &[u8], begin: usize, available: usize) -> (u16, usize) {
    let byte = |offset: usize| text.get(begin + offset).copied().unwrap_or(0);
    let first = byte(0);
    if first < 0x80 {
        (u16::from(first), 1)
    } else if available >= 2 && first & 0xe0 == 0xc0 {
        (u16::from(first & 0x1f) << 6 | u16::from(byte(1) & 0x3f), 2)
    } else if available >= 3 && first & 0xf0 == 0xe0 {
        let code = u16::from(first & 0x0f) << 12
            | u16::from(byte(1) & 0x3f) << 6
            | u16::from(byte(2) & 0x3f);
        (code, 3)
    } else if available >= 4 && first & 0xf8 == 0xf0 {
        (0, 4)
    } else if available >= 5 && first & 0xfc == 0xf8 {
        (0, 5)
    } else if available >= 6 && first & 0xfe == 0xfc {
        (0, 6)
    } else {
        (0, 1)
    }
}
