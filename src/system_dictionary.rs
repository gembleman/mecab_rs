use std::fmt;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::Path;

use crate::error::{Error, IoAction, io_error};
use crate::mapped_file::MappedFile;
use crate::section::{Plain, Section};
use crate::utf8::is_continuation;

mod check;
mod header;

use check::{Checked, check_last_feature_byte};
use header::Header;

pub const MAGIC_ID: u32 = 0xEF71_8F77;
pub const DIC_VERSION: u32 = 102;
pub const HEADER_SIZE: usize = 72;
pub const TOKEN_SIZE: usize = 16;
pub const VALUE_SIZE_BITS: u32 = 8;
const VALUE_SIZE_MASK: i32 = 0xff;
pub(crate) const OUTSIDE_TOKEN_TABLE: &str = "a double array entry points outside the token table";
const VERSION_AT: usize = 4;
const KIND_AT: usize = 8;
const LEFT_SIZE_AT: usize = 16;
const RIGHT_SIZE_AT: usize = 20;
pub(crate) const ARRAY_BYTES_AT: usize = 24;
pub(crate) const TOKEN_BYTES_AT: usize = 28;
pub(crate) const FEATURE_BYTES_AT: usize = 32;
pub(crate) const CHARSET_AT: usize = 40;
pub const CHARSET_BYTES: usize = HEADER_SIZE - CHARSET_AT;
const CHARSET: Range<usize> = CHARSET_AT..HEADER_SIZE;
const UTF8_NAMES: [&[u8]; 2] = [b"utf-8", b"utf8"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictionaryKind {
    System = 0,
    User = 1,
    Unknown = 2,
}

fn charset_name(header: &[u8]) -> &[u8] {
    header[CHARSET]
        .split(|&byte| byte == 0)
        .next()
        .unwrap_or_default()
}

fn is_utf8_charset(name: &[u8]) -> bool {
    UTF8_NAMES
        .iter()
        .any(|utf8| name.eq_ignore_ascii_case(utf8))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefixHit {
    pub(crate) first_token: usize,
    pub(crate) token_count: usize,
    pub(crate) length: usize,
}

impl PrefixHit {
    fn new(value: i32, length: usize) -> Self {
        Self {
            first_token: (value >> VALUE_SIZE_BITS).cast_unsigned() as usize,
            token_count: (value & VALUE_SIZE_MASK).cast_unsigned() as usize,
            length,
        }
    }

    pub fn tokens(self) -> Range<usize> {
        self.first_token..self.first_token + self.token_count
    }

    pub fn length(self) -> usize {
        self.length
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TokenRecord {
    pub left_id: u16,
    pub right_id: u16,
    pub pos_id: u16,
    pub word_cost: i16,
    pub feature: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct RawToken {
    word: u32,
    costs: u32,
    feature: u32,
    reserved: u32,
}

impl RawToken {
    fn record(&self) -> TokenRecord {
        TokenRecord {
            left_id: self.word as u16,
            right_id: (self.word >> 16) as u16,
            pos_id: self.costs as u16,
            word_cost: ((self.costs >> 16) as u16).cast_signed(),
            feature: self.feature,
        }
    }
}

const _: () = assert!(size_of::<RawToken>() == TOKEN_SIZE);

unsafe impl Plain for RawToken {}

impl fmt::Debug for SystemDictionary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SystemDictionary")
            .field("map_bytes", &self.map.len())
            .field("left_size", &self.left_size)
            .field("right_size", &self.right_size)
            .finish_non_exhaustive()
    }
}

pub struct SystemDictionary {
    map: MappedFile,
    left_size: usize,
    right_size: usize,
    cells: Section<[u32; 2]>,
    tokens: Section<RawToken>,
    features: Range<usize>,
}

impl SystemDictionary {
    pub fn open(path: &Path, kind: DictionaryKind) -> Result<Self, Error> {
        Self::open_checking_in_blocks(path, kind, CHECK_BLOCK_BYTES)
    }

    pub fn open_checking_in_blocks(
        path: &Path,
        kind: DictionaryKind,
        read_block_bytes: usize,
    ) -> Result<Self, Error> {
        let io_error = io_error(IoAction::Read, path);
        let map = MappedFile::open(path)?;
        let broken = |reason| Error::format(path, reason);
        let bytes = &map[..];
        let header = Header::parse(bytes, kind, path)?;
        let cells = Section::new(bytes, header.cells())
            .ok_or_else(|| broken("the double array is misaligned"))?;
        let tokens = Section::new(bytes, header.tokens())
            .ok_or_else(|| broken("the token table is misaligned"))?;
        check_last_feature_byte(map.file(), &header, path)?;
        Checked::new(map.file(), &header, read_block_bytes)
            .run()
            .map_err(&io_error)?
            .map_err(broken)?;
        Ok(Self {
            map,
            left_size: header.left_size,
            right_size: header.right_size,
            cells,
            tokens,
            features: header.features(),
        })
    }

    pub fn charset(&self) -> &[u8] {
        charset_name(&self.map)
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<PrefixHit>) {
        hits.clear();
        let cells = self.cells.view(&self.map);
        let Some(&[root, _]) = cells.first() else {
            return;
        };
        let mut base = root.cast_signed() as isize;
        for (consumed, &byte) in key.iter().enumerate() {
            push_terminal(cells, base, consumed, hits);
            match cells.get(child_index(base, byte)) {
                Some(&[next, check]) if base.cast_unsigned() as u32 == check => {
                    base = next.cast_signed() as isize;
                }
                _ => return,
            }
        }
        push_terminal(cells, base, key.len(), hits);
    }

    pub fn exact_match(&self, key: &[u8]) -> Option<Range<usize>> {
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
        let found = base.cast_unsigned() == check && value < 0;
        found.then(|| PrefixHit::new(!value, key.len()).tokens())
    }

    pub fn token(&self, index: usize) -> TokenRecord {
        self.tokens.view(&self.map)[index].record()
    }

    pub fn feature(&self, offset: u32) -> &str {
        let rest = &self.map[self.features.start + offset as usize..self.features.end];
        assert!(
            rest.first().is_some_and(|&byte| !is_continuation(byte)),
            "feature offset is not at a character start"
        );
        let length = rest
            .iter()
            .position(|&byte| byte == 0)
            .expect("open checked that the features end with a NUL byte");
        unsafe { str::from_utf8_unchecked(&rest[..length]) }
    }
}

pub(crate) const CHECK_BLOCK_BYTES: usize = 1 << 18;
pub(crate) const CELL_SIZE: usize = 8;

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        #[cfg(windows)]
        let count = std::os::windows::fs::FileExt::seek_read(file, buf, offset)?;
        #[cfg(unix)]
        let count = std::os::unix::fs::FileExt::read_at(file, buf, offset)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[count..];
        offset += count as u64;
    }
    Ok(())
}

fn cell(cells: &[[u32; 2]], index: i64) -> Option<(i32, u32)> {
    let [value, check] = *cells.get(usize::try_from(index).ok()?)?;
    Some((value.cast_signed(), check))
}

#[inline(always)]
fn child_index(base: isize, byte: u8) -> usize {
    base.wrapping_add(byte as isize + 1).cast_unsigned()
}

#[inline]
fn push_terminal(cells: &[[u32; 2]], base: isize, length: usize, hits: &mut Vec<PrefixHit>) {
    if let Some(&[value, check]) = cells.get(base.cast_unsigned())
        && value.cast_signed() < 0
        && base.cast_unsigned() as u32 == check
    {
        hits.push(PrefixHit::new(!value.cast_signed(), length));
    }
}
