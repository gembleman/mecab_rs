use std::ops::Range;
use std::path::Path;

use memmap2::Mmap;

use crate::dictionary::map_file;
use crate::error::Error;
use crate::section::{Plain, Section};

const MAGIC_ID: u32 = 0xEF71_8F77;
const DIC_VERSION: u32 = 102;
const HEADER_SIZE: usize = 72;
const TOKEN_SIZE: usize = 16;
const MIN_FILE_SIZE: usize = 100;
const VALUE_SIZE_BITS: u32 = 8;
const VALUE_SIZE_MASK: i32 = 0xff;
const MAX_PREFIX_HITS: usize = 512;
const VERSION_AT: usize = 4;
const KIND_AT: usize = 8;
const LEFT_SIZE_AT: usize = 16;
const RIGHT_SIZE_AT: usize = 20;
const ARRAY_BYTES_AT: usize = 24;
const TOKEN_BYTES_AT: usize = 28;
const FEATURE_BYTES_AT: usize = 32;
const CHARSET: Range<usize> = 40..HEADER_SIZE;
const UTF8_NAMES: [&[u8]; 2] = [b"utf-8", b"utf8"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DictionaryKind {
    System = 0,
    Unknown = 2,
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(*bytes[at..].first_chunk().expect("the header is long enough"))
}

fn is_utf8_charset(field: &[u8]) -> bool {
    let name = field.split(|&byte| byte == 0).next().unwrap_or_default();
    UTF8_NAMES.iter().any(|utf8| name.eq_ignore_ascii_case(utf8))
}

#[derive(Clone, Copy)]
pub(crate) struct PrefixHit {
    pub(crate) value: i32,
    pub(crate) length: usize,
}

impl PrefixHit {
    pub(crate) fn first_token(self) -> usize {
        (self.value >> VALUE_SIZE_BITS).cast_unsigned() as usize
    }

    pub(crate) fn token_count(self) -> usize {
        (self.value & VALUE_SIZE_MASK).cast_unsigned() as usize
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TokenRecord {
    pub(crate) lc_attr: u16,
    pub(crate) rc_attr: u16,
    pub(crate) pos_id: u16,
    pub(crate) wcost: i16,
    pub(crate) feature: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct RawToken {
    word: u32,
    costs: u32,
    feature: u32,
    reserved: u32,
}

const _: () = assert!(size_of::<RawToken>() == TOKEN_SIZE);

unsafe impl Plain for RawToken {}

pub(crate) struct SystemDictionary {
    map: Mmap,
    left_size: usize,
    right_size: usize,
    cells: Section<[u32; 2]>,
    tokens: Section<RawToken>,
    features: Range<usize>,
}

impl SystemDictionary {
    pub(crate) fn open(path: &Path, kind: DictionaryKind) -> Result<Self, Error> {
        let map = map_file(path)?;
        let broken = |reason| Error::Format { file: path.to_path_buf(), reason };
        let bytes = &map[..];
        if bytes.len() < MIN_FILE_SIZE
            || (read_u32(bytes, 0) ^ MAGIC_ID) as usize != bytes.len()
            || read_u32(bytes, VERSION_AT) != DIC_VERSION
        {
            return Err(broken("the header, magic number or version is not valid"));
        }
        if read_u32(bytes, KIND_AT) != kind as u32 {
            return Err(broken("the dictionary type does not match the file name"));
        }
        if !is_utf8_charset(&bytes[CHARSET]) {
            return Err(Error::Unsupported("a dictionary charset other than UTF-8"));
        }
        let array_end = HEADER_SIZE + read_u32(bytes, ARRAY_BYTES_AT) as usize;
        let tokens_end = array_end + read_u32(bytes, TOKEN_BYTES_AT) as usize;
        let features_end = tokens_end + read_u32(bytes, FEATURE_BYTES_AT) as usize;
        if features_end != bytes.len() {
            return Err(broken("the section sizes do not add up to the file size"));
        }
        let left_size = read_u32(bytes, LEFT_SIZE_AT) as usize;
        let right_size = read_u32(bytes, RIGHT_SIZE_AT) as usize;
        let cells = Section::new(bytes, HEADER_SIZE..array_end)
            .ok_or_else(|| broken("the double array is misaligned"))?;
        let tokens = Section::new(bytes, array_end..tokens_end)
            .ok_or_else(|| broken("the token table is misaligned"))?;
        Ok(Self { map, left_size, right_size, cells, tokens, features: tokens_end..features_end })
    }

    pub(crate) fn left_size(&self) -> usize {
        self.left_size
    }

    pub(crate) fn right_size(&self) -> usize {
        self.right_size
    }

    pub(crate) fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<PrefixHit>) {
        hits.clear();
        let cells = self.cells.view(&self.map);
        let Some((mut base, _)) = cell(cells, 0) else {
            return;
        };
        for (consumed, &byte) in key.iter().enumerate() {
            push_terminal(cells, base, consumed, hits);
            match cell(cells, i64::from(base) + i64::from(byte) + 1) {
                Some((next, check)) if base.cast_unsigned() == check => base = next,
                _ => return,
            }
        }
        push_terminal(cells, base, key.len(), hits);
    }

    pub(crate) fn exact_match(&self, key: &[u8]) -> Option<i32> {
        let cells = self.cells.view(&self.map);
        let (mut base, _) = cell(cells, 0)?;
        for &byte in key {
            let (next, check) = cell(cells, i64::from(base) + i64::from(byte) + 1)?;
            if base.cast_unsigned() != check {
                return None;
            }
            base = next;
        }
        let (value, check) = cell(cells, i64::from(base))?;
        (base.cast_unsigned() == check && value < 0).then_some(-value - 1)
    }

    pub(crate) fn token(&self, index: usize) -> Result<TokenRecord, Error> {
        let raw = self.tokens.view(&self.map).get(index).ok_or(Error::Corrupt("a token index"))?;
        let token = TokenRecord {
            lc_attr: raw.word as u16,
            rc_attr: (raw.word >> 16) as u16,
            pos_id: raw.costs as u16,
            wcost: ((raw.costs >> 16) as u16).cast_signed(),
            feature: raw.feature,
        };
        let connectable =
            usize::from(token.lc_attr) < self.right_size && usize::from(token.rc_attr) < self.left_size;
        if !connectable {
            return Err(Error::Corrupt("a token context id outside the matrix"));
        }
        if raw.feature as usize >= self.features.len() {
            return Err(Error::Corrupt("a token feature offset"));
        }
        Ok(token)
    }

    pub(crate) fn feature(&self, offset: u32) -> &[u8] {
        let rest = &self.map[self.features.start + offset as usize..self.features.end];
        let length = rest.iter().position(|&byte| byte == 0).unwrap_or(rest.len());
        &rest[..length]
    }
}

fn cell(cells: &[[u32; 2]], index: i64) -> Option<(i32, u32)> {
    let [value, check] = *cells.get(usize::try_from(index).ok()?)?;
    Some((value.cast_signed(), check))
}

fn push_terminal(cells: &[[u32; 2]], base: i32, length: usize, hits: &mut Vec<PrefixHit>) {
    if let Some((value, check)) = cell(cells, i64::from(base))
        && value < 0
        && base.cast_unsigned() == check
        && hits.len() < MAX_PREFIX_HITS
    {
        hits.push(PrefixHit { value: -value - 1, length });
    }
}
