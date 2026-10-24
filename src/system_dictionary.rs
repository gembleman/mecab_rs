use std::ops::Range;
use std::path::Path;
use std::thread;

use memmap2::Mmap;

use crate::dictionary::map_file;
use crate::error::Error;
use crate::section::{Plain, Section};

pub(crate) const MAGIC_ID: u32 = 0xEF71_8F77;
pub(crate) const DIC_VERSION: u32 = 102;
pub(crate) const HEADER_SIZE: usize = 72;
pub(crate) const TOKEN_SIZE: usize = 16;
const VALUE_SIZE_BITS: u32 = 8;
const VALUE_SIZE_MASK: i32 = 0xff;
const OUTSIDE_TOKEN_TABLE: &str = "a double array entry points outside the token table";
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
    User = 1,
    Unknown = 2,
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(
        *bytes[at..]
            .first_chunk()
            .expect("the header is long enough"),
    )
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

#[derive(Clone, Copy)]
pub(crate) struct PrefixHit {
    first_token: usize,
    token_count: usize,
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

    pub(crate) fn tokens(self) -> Range<usize> {
        self.first_token..self.first_token + self.token_count
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TokenRecord {
    pub(crate) left_id: u16,
    pub(crate) right_id: u16,
    pub(crate) pos_id: u16,
    pub(crate) word_cost: i16,
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

    fn is_valid(&self, left_size: usize, right_size: usize, features: &[u8]) -> bool {
        let token = self.record();
        usize::from(token.left_id) < right_size
            && usize::from(token.right_id) < left_size
            && features
                .get(token.feature as usize)
                .is_some_and(|&byte| byte & 0xC0 != 0x80)
    }
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
        let broken = |reason| Error::Format {
            file: path.to_path_buf(),
            reason,
        };
        let bytes = &map[..];
        if bytes.len() < HEADER_SIZE
            || (read_u32(bytes, 0) ^ MAGIC_ID) as usize != bytes.len()
            || read_u32(bytes, VERSION_AT) != DIC_VERSION
        {
            return Err(broken("the header, magic number or version is not valid"));
        }
        if read_u32(bytes, KIND_AT) != kind as u32 {
            return Err(broken("the dictionary type is not the expected one"));
        }
        if !is_utf8_charset(charset_name(bytes)) {
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
        let features = &bytes[tokens_end..features_end];
        if features.last().is_some_and(|&last| last != 0) {
            return Err(broken("the features do not end with a NUL byte"));
        }
        let checked = Checked {
            cells: cells.view(&map),
            tokens: tokens.view(&map),
            features,
            left_size,
            right_size,
        };
        checked.run().map_err(broken)?;
        Ok(Self {
            map,
            left_size,
            right_size,
            cells,
            tokens,
            features: tokens_end..features_end,
        })
    }

    pub(crate) fn charset(&self) -> &[u8] {
        charset_name(&self.map)
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
        let Some(&[root, _]) = cells.first() else {
            return;
        };
        let mut base = root.cast_signed() as isize;
        for (consumed, &byte) in key.iter().enumerate() {
            push_terminal(cells, base, consumed, hits);
            match cells.get(base.wrapping_add(byte as isize + 1).cast_unsigned()) {
                Some(&[next, check]) if base.cast_unsigned() as u32 == check => {
                    base = next.cast_signed() as isize;
                }
                _ => return,
            }
        }
        push_terminal(cells, base, key.len(), hits);
    }

    pub(crate) fn exact_match(&self, key: &[u8]) -> Option<Range<usize>> {
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
        found.then(|| PrefixHit::new(-value - 1, key.len()).tokens())
    }

    pub(crate) fn token(&self, index: usize) -> TokenRecord {
        self.tokens.view(&self.map)[index].record()
    }

    pub(crate) fn feature(&self, offset: u32) -> &str {
        let rest = &self.map[self.features.start + offset as usize..self.features.end];
        let length = rest
            .iter()
            .position(|&byte| byte == 0)
            .expect("open checked that the features end with a NUL byte");
        unsafe { str::from_utf8_unchecked(&rest[..length]) }
    }
}

const PARALLEL_MIN_BYTES: usize = 1 << 22;
const MAX_CHECK_THREADS: usize = 8;

struct Checked<'a> {
    cells: &'a [[u32; 2]],
    tokens: &'a [RawToken],
    features: &'a [u8],
    left_size: usize,
    right_size: usize,
}

impl Checked<'_> {
    fn run(&self) -> Result<(), &'static str> {
        let threads = if self.features.len() < PARALLEL_MIN_BYTES {
            1
        } else {
            thread::available_parallelism().map_or(1, |count| count.get().min(MAX_CHECK_THREADS))
        };
        let pieces = self.feature_pieces(threads);
        let token_chunk = self.tokens.len().div_ceil(threads).max(1);
        let cell_chunk = self.cells.len().div_ceil(threads).max(1);
        let (utf8, contexts, terminals) = if threads == 1 {
            (
                pieces.iter().all(|piece| str::from_utf8(piece).is_ok()),
                self.tokens_valid(self.tokens),
                self.terminals_valid(self.cells, 0),
            )
        } else {
            thread::scope(|scope| {
                let utf8: Vec<_> = pieces
                    .iter()
                    .map(|piece| scope.spawn(|| str::from_utf8(piece).is_ok()))
                    .collect();
                let contexts: Vec<_> = self
                    .tokens
                    .chunks(token_chunk)
                    .map(|chunk| scope.spawn(|| self.tokens_valid(chunk)))
                    .collect();
                let terminals: Vec<_> = self
                    .cells
                    .chunks(cell_chunk)
                    .enumerate()
                    .map(|(number, chunk)| {
                        scope.spawn(move || self.terminals_valid(chunk, number * cell_chunk))
                    })
                    .collect();
                let joined = |handle: thread::ScopedJoinHandle<bool>| {
                    handle.join().expect("a check thread panicked")
                };
                (
                    utf8.into_iter().all(joined),
                    contexts.into_iter().all(joined),
                    terminals.into_iter().all(joined),
                )
            })
        };
        if !utf8 {
            return Err("the features are not valid UTF-8");
        }
        if !contexts {
            return Err(
                "a token context id is out of range or a feature offset is not at a character boundary",
            );
        }
        if !terminals {
            return Err(OUTSIDE_TOKEN_TABLE);
        }
        Ok(())
    }

    fn terminals_valid(&self, cells: &[[u32; 2]], first: usize) -> bool {
        cells.iter().enumerate().all(|(offset, &[value, check])| {
            let value = value.cast_signed();
            value >= 0
                || check as usize != first + offset
                || PrefixHit::new(!value, 0).tokens().end <= self.tokens.len()
        })
    }

    fn feature_pieces(&self, count: usize) -> Vec<&[u8]> {
        let mut pieces = Vec::with_capacity(count);
        let mut start = 0;
        for part in 1..count {
            let target = self.features.len() / count * part;
            if target <= start {
                continue;
            }
            let end = self.features[target..]
                .iter()
                .position(|&byte| byte == 0)
                .map_or(self.features.len(), |at| target + at + 1);
            if end > start {
                pieces.push(&self.features[start..end]);
                start = end;
            }
        }
        pieces.push(&self.features[start..]);
        pieces
    }

    fn tokens_valid(&self, tokens: &[RawToken]) -> bool {
        tokens
            .iter()
            .all(|raw| raw.is_valid(self.left_size, self.right_size, self.features))
    }
}

fn cell(cells: &[[u32; 2]], index: i64) -> Option<(i32, u32)> {
    let [value, check] = *cells.get(usize::try_from(index).ok()?)?;
    Some((value.cast_signed(), check))
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
