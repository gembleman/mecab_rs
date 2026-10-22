use std::ops::Range;
use std::path::Path;

use memmap2::Mmap;

use crate::dictionary::{map_file, mapped_view};
use crate::error::Error;

const MAGIC_ID: u32 = 0xEF71_8F77;
const DIC_VERSION: u32 = 102;
const HEADER_SIZE: usize = 72;
const TOKEN_SIZE: usize = 16;
const MIN_FILE_SIZE: usize = 100;
const VALUE_SIZE_BITS: u32 = 8;
const VALUE_SIZE_MASK: i32 = 0xff;
const MAX_PREFIX_HITS: usize = 512;
const VERSION_AT: usize = 4;
const LEFT_SIZE_AT: usize = 16;
const RIGHT_SIZE_AT: usize = 20;
const ARRAY_BYTES_AT: usize = 24;
const TOKEN_BYTES_AT: usize = 28;
const FEATURE_BYTES_AT: usize = 32;

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[derive(Clone, Copy)]
pub(crate) struct PrefixHit {
    pub(crate) value: i32,
    pub(crate) length: usize,
}

impl PrefixHit {
    pub(crate) fn first_token(self) -> usize {
        (self.value >> VALUE_SIZE_BITS) as usize
    }

    pub(crate) fn token_count(self) -> usize {
        (self.value & VALUE_SIZE_MASK) as usize
    }
}

#[derive(Clone, Copy, Default)]
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

const _: () = assert!(std::mem::size_of::<RawToken>() == TOKEN_SIZE);

pub(crate) struct SystemDictionary {
    map: Mmap,
    left_size: usize,
    right_size: usize,
    cells: &'static [[u32; 2]],
    tokens: &'static [RawToken],
    features: Range<usize>,
}

impl SystemDictionary {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let map = map_file(path)?;
        let broken = |reason| Error::Format { file: path.to_path_buf(), reason };
        let bytes = &map[..];
        if bytes.len() < MIN_FILE_SIZE
            || (read_u32(bytes, 0) ^ MAGIC_ID) as usize != bytes.len()
            || read_u32(bytes, VERSION_AT) != DIC_VERSION
        {
            return Err(broken("the header, magic number or version is not valid"));
        }
        let array_end = HEADER_SIZE + read_u32(bytes, ARRAY_BYTES_AT) as usize;
        let tokens_end = array_end + read_u32(bytes, TOKEN_BYTES_AT) as usize;
        let features_end = tokens_end + read_u32(bytes, FEATURE_BYTES_AT) as usize;
        if features_end != bytes.len() {
            return Err(broken("the section sizes do not add up to the file size"));
        }
        let left_size = read_u32(bytes, LEFT_SIZE_AT) as usize;
        let right_size = read_u32(bytes, RIGHT_SIZE_AT) as usize;
        let cells = mapped_view(&bytes[HEADER_SIZE..array_end])
            .ok_or_else(|| broken("the double array is misaligned"))?;
        let tokens = mapped_view(&bytes[array_end..tokens_end])
            .ok_or_else(|| broken("the token table is misaligned"))?;
        Ok(Self { map, left_size, right_size, cells, tokens, features: tokens_end..features_end })
    }

    pub(crate) fn left_size(&self) -> usize {
        self.left_size
    }

    pub(crate) fn right_size(&self) -> usize {
        self.right_size
    }

    fn cell(&self, index: i64) -> Option<(i32, u32)> {
        let [value, check] = *self.cells.get(usize::try_from(index).ok()?)?;
        Some((value as i32, check))
    }

    pub(crate) fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<PrefixHit>) {
        hits.clear();
        let Some((mut base, _)) = self.cell(0) else {
            return;
        };
        for (consumed, &byte) in key.iter().enumerate() {
            self.push_terminal(base, consumed, hits);
            match self.cell(i64::from(base) + i64::from(byte) + 1) {
                Some((next, check)) if base as u32 == check => base = next,
                _ => return,
            }
        }
        self.push_terminal(base, key.len(), hits);
    }

    fn push_terminal(&self, base: i32, length: usize, hits: &mut Vec<PrefixHit>) {
        if let Some((value, check)) = self.cell(i64::from(base))
            && value < 0
            && base as u32 == check
            && hits.len() < MAX_PREFIX_HITS
        {
            hits.push(PrefixHit { value: -value - 1, length });
        }
    }

    pub(crate) fn exact_match(&self, key: &[u8]) -> Option<i32> {
        let (mut base, _) = self.cell(0)?;
        for &byte in key {
            let (next, check) = self.cell(i64::from(base) + i64::from(byte) + 1)?;
            if base as u32 != check {
                return None;
            }
            base = next;
        }
        let (value, check) = self.cell(i64::from(base))?;
        (base as u32 == check && value < 0).then_some(-value - 1)
    }

    pub(crate) fn token(&self, index: usize) -> TokenRecord {
        let Some(raw) = self.tokens.get(index) else {
            return TokenRecord::default();
        };
        TokenRecord {
            lc_attr: raw.word as u16,
            rc_attr: (raw.word >> 16) as u16,
            pos_id: raw.costs as u16,
            wcost: (raw.costs >> 16) as i16,
            feature: raw.feature,
        }
    }

    pub(crate) fn feature(&self, offset: u32) -> &[u8] {
        let start = self.features.start + offset as usize;
        let Some(rest) = self.map.get(start..self.features.end) else {
            return &[];
        };
        let length = rest.iter().position(|&byte| byte == 0).unwrap_or(rest.len());
        &rest[..length]
    }
}
