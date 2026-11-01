use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::thread;

use super::header::Header;
use super::{CELL_SIZE, OUTSIDE_TOKEN_TABLE, PrefixHit, RawToken, TOKEN_SIZE, read_exact_at};
use crate::error::{Error, IoAction, io_error};
use crate::utf8::is_continuation;
use crate::wire::u32_at;

const PARALLEL_MIN_BYTES: usize = 1 << 22;
const MAX_CHECK_THREADS: usize = 8;

pub(super) fn check_last_feature_byte(
    file: &File,
    header: &Header,
    path: &Path,
) -> Result<(), Error> {
    if header.features().end <= header.tokens().end {
        return Ok(());
    }
    let mut last_feature_byte = [0];
    read_exact_at(
        file,
        &mut last_feature_byte,
        header.features().end as u64 - 1,
    )
    .map_err(io_error(IoAction::Read, path))?;
    if last_feature_byte[0] != 0 {
        return Err(Error::format(
            path,
            "the features do not end with a NUL byte",
        ));
    }
    Ok(())
}

pub(super) struct Checked<'a> {
    file: &'a File,
    cells: Range<usize>,
    tokens: Range<usize>,
    features: Range<usize>,
    left_size: usize,
    right_size: usize,
    read_block_bytes: usize,
}

impl<'a> Checked<'a> {
    pub(super) fn new(file: &'a File, header: &Header, read_block_bytes: usize) -> Self {
        Self {
            file,
            cells: header.cells(),
            tokens: header.tokens(),
            features: header.features(),
            left_size: header.left_size,
            right_size: header.right_size,
            read_block_bytes: read_block_bytes.max(1),
        }
    }
}

impl Checked<'_> {
    pub(super) fn run(&self) -> io::Result<Result<(), &'static str>> {
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
                    word: u32_at(raw, 0),
                    costs: u32_at(raw, 4),
                    feature: u32_at(raw, 8),
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
                    let value = u32_at(cell, 0).cast_signed();
                    let check = u32_at(cell, 4) as usize;
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
                boundaries &= !is_continuation(buf[offset as usize - buf_start]);
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
