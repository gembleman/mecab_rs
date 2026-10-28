use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::thread;

use memmap2::Mmap;

use crate::dictionary::map_open_file;
use crate::error::Error;
use crate::section::{Plain, Section};

pub(crate) const MAGIC_ID: u32 = 0xEF71_8F77;
pub(crate) const DIC_VERSION: u32 = 102;
pub(crate) const HEADER_SIZE: usize = 72;
pub(crate) const TOKEN_SIZE: usize = 16;
pub(crate) const VALUE_SIZE_BITS: u32 = 8;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefixHit {
    pub(crate) first_token: usize,
    pub(crate) token_count: usize,
    pub length: usize,
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

#[derive(Clone, Copy)]
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
        Self::open_checking_in_blocks(path, kind, CHECK_BLOCK_BYTES)
    }

    fn open_checking_in_blocks(
        path: &Path,
        kind: DictionaryKind,
        read_block_bytes: usize,
    ) -> Result<Self, Error> {
        let io_error = |source| Error::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = File::open(path).map_err(io_error)?;
        let map = map_open_file(&file, path)?;
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
        if features_end > tokens_end {
            let mut last_feature_byte = [0];
            read_exact_at(&file, &mut last_feature_byte, features_end as u64 - 1)
                .map_err(io_error)?;
            if last_feature_byte[0] != 0 {
                return Err(broken("the features do not end with a NUL byte"));
            }
        }
        let checked = Checked {
            file: &file,
            cells: HEADER_SIZE..array_end,
            tokens: array_end..tokens_end,
            features: tokens_end..features_end,
            left_size,
            right_size,
            read_block_bytes,
        };
        checked.run().map_err(io_error)?.map_err(broken)?;
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
            match cells.get(child_index(base, byte)) {
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
const CHECK_BLOCK_BYTES: usize = 1 << 18;
const CELL_SIZE: usize = 8;

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

struct Checked<'a> {
    file: &'a File,
    cells: Range<usize>,
    tokens: Range<usize>,
    features: Range<usize>,
    left_size: usize,
    right_size: usize,
    read_block_bytes: usize,
}

impl Checked<'_> {
    fn run(&self) -> io::Result<Result<(), &'static str>> {
        let threads = if self.features.len() < PARALLEL_MIN_BYTES {
            1
        } else {
            thread::available_parallelism().map_or(1, |count| count.get().min(MAX_CHECK_THREADS))
        };
        let (tokens, cells) = in_parallel(
            threads,
            |part| self.tokens_part(part, threads),
            |part| self.terminals_part(part, threads),
        );
        let mut offsets = Vec::new();
        let mut contexts_in_range = true;
        for result in tokens {
            let (valid, part) = result?;
            contexts_in_range &= valid;
            offsets.extend(part);
        }
        let mut terminals_in_token_table = true;
        for result in cells {
            terminals_in_token_table &= result?;
        }
        offsets.sort_unstable();
        offsets.dedup();
        contexts_in_range &= offsets
            .last()
            .is_none_or(|&last| (last as usize) < self.features.len());
        let (features, _) = in_parallel(
            threads,
            |part| self.features_part(part, threads, &offsets),
            |_| (),
        );
        let mut features_are_utf8 = true;
        for result in features {
            let (valid, boundaries) = result?;
            features_are_utf8 &= valid;
            contexts_in_range &= boundaries;
        }
        if !features_are_utf8 {
            return Ok(Err("the features are not valid UTF-8"));
        }
        if !contexts_in_range {
            return Ok(Err(
                "a token context id is out of range or a feature offset is not at a character boundary",
            ));
        }
        if !terminals_in_token_table {
            return Ok(Err(OUTSIDE_TOKEN_TABLE));
        }
        Ok(Ok(()))
    }

    fn each_block(
        &self,
        section: usize,
        size: usize,
        range: Range<usize>,
        mut check: impl FnMut(usize, &[u8]) -> bool,
    ) -> io::Result<bool> {
        let file = self.file;
        let per_block = (self.read_block_bytes / size).max(1);
        let mut buf = vec![0; per_block.min(range.len()) * size];
        let mut index = range.start;
        while index < range.end {
            let count = per_block.min(range.end - index);
            let bytes = &mut buf[..count * size];
            read_exact_at(file, bytes, (section + index * size) as u64)?;
            if !check(index, bytes) {
                return Ok(false);
            }
            index += count;
        }
        Ok(true)
    }

    fn tokens_part(&self, part: usize, parts: usize) -> io::Result<(bool, Vec<u32>)> {
        let range = part_of(self.tokens.len() / TOKEN_SIZE, part, parts);
        let mut offsets = Vec::with_capacity(range.len());
        let valid = self.each_block(self.tokens.start, TOKEN_SIZE, range, |_, bytes| {
            bytes.as_chunks::<TOKEN_SIZE>().0.iter().all(|raw| {
                let token = RawToken {
                    word: read_u32(raw, 0),
                    costs: read_u32(raw, 4),
                    feature: read_u32(raw, 8),
                    reserved: 0,
                }
                .record();
                offsets.push(token.feature);
                usize::from(token.left_id) < self.right_size
                    && usize::from(token.right_id) < self.left_size
            })
        })?;
        Ok((valid, offsets))
    }

    fn terminals_part(&self, part: usize, parts: usize) -> io::Result<bool> {
        let token_count = self.tokens.len() / TOKEN_SIZE;
        let range = part_of(self.cells.len() / CELL_SIZE, part, parts);
        self.each_block(self.cells.start, CELL_SIZE, range, |first, bytes| {
            bytes
                .as_chunks::<CELL_SIZE>()
                .0
                .iter()
                .enumerate()
                .all(|(offset, cell)| {
                    let value = read_u32(cell, 0).cast_signed();
                    let check = read_u32(cell, 4) as usize;
                    let is_terminal = value < 0 && check == first + offset;
                    let hit_value = !value;
                    !is_terminal || PrefixHit::new(hit_value, 0).tokens().end <= token_count
                })
        })
    }

    fn after_nul(&self, file: &File, from: usize) -> io::Result<usize> {
        let length = self.features.len();
        let mut buf = vec![0; self.read_block_bytes.min(length - from)];
        let mut at = from;
        while at < length {
            let count = buf.len().min(length - at);
            read_exact_at(file, &mut buf[..count], (self.features.start + at) as u64)?;
            if let Some(nul) = buf[..count].iter().position(|&byte| byte == 0) {
                return Ok(at + nul + 1);
            }
            at += count;
        }
        Ok(length)
    }

    fn features_part(
        &self,
        part: usize,
        parts: usize,
        offsets: &[u32],
    ) -> io::Result<(bool, bool)> {
        let file = self.file;
        let length = self.features.len();
        let start = match part {
            0 => 0,
            _ => self.after_nul(file, length / parts * part)?,
        };
        let end = if part + 1 == parts {
            length
        } else {
            self.after_nul(file, length / parts * (part + 1))?
        };
        let offsets = &offsets[offsets.partition_point(|&offset| (offset as usize) < start)..];
        let offsets = &offsets[..offsets.partition_point(|&offset| (offset as usize) < end)];
        let mut next = offsets.iter().peekable();
        let (mut features_are_utf8, mut boundaries) = (true, true);
        let mut buf = Vec::new();
        let mut position = start;
        while position < end {
            let count = self.read_block_bytes.min(end - position);
            let buf_start = position - buf.len();
            let old = buf.len();
            buf.resize(old + count, 0);
            read_exact_at(
                file,
                &mut buf[old..],
                (self.features.start + position) as u64,
            )?;
            position += count;
            while let Some(&&offset) = next.peek()
                && (offset as usize) < position
            {
                boundaries &= buf[offset as usize - buf_start] & 0xC0 != 0x80;
                next.next();
            }
            let complete = buf
                .iter()
                .rposition(|&byte| byte == 0)
                .map_or(0, |at| at + 1);
            features_are_utf8 &= str::from_utf8(&buf[..complete]).is_ok();
            buf.drain(..complete);
            if !features_are_utf8 || !boundaries {
                return Ok((features_are_utf8, boundaries));
            }
        }
        features_are_utf8 &= str::from_utf8(&buf).is_ok();
        Ok((features_are_utf8, boundaries))
    }
}

fn part_of(count: usize, part: usize, parts: usize) -> Range<usize> {
    let chunk = count.div_ceil(parts).max(1);
    (part * chunk).min(count)..((part + 1) * chunk).min(count)
}

fn join<T>(handle: thread::ScopedJoinHandle<T>) -> T {
    handle.join().expect("a check thread panicked")
}

fn in_parallel<A: Send, B: Send>(
    threads: usize,
    first: impl Fn(usize) -> A + Sync,
    second: impl Fn(usize) -> B + Sync,
) -> (Vec<A>, Vec<B>) {
    if threads == 1 {
        return (vec![first(0)], vec![second(0)]);
    }
    thread::scope(|scope| {
        let (first, second) = (&first, &second);
        let firsts: Vec<_> = (0..threads)
            .map(|part| scope.spawn(move || first(part)))
            .collect();
        let seconds: Vec<_> = (0..threads)
            .map(|part| scope.spawn(move || second(part)))
            .collect();
        (
            firsts.into_iter().map(join).collect(),
            seconds.into_iter().map(join).collect(),
        )
    })
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
