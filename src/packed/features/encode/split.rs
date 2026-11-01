use std::thread;

use super::lanes::lane_count;
use crate::Error;

const PARALLEL_MIN_BYTES: usize = 1 << 22;

struct Chunk<'a> {
    first: Option<usize>,
    last: usize,
    rest: Vec<&'a [u8]>,
}

pub(super) fn split_entries(features: &[u8]) -> Result<Vec<&[u8]>, Error> {
    if u32::try_from(features.len()).is_err() {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }
    let parts = if features.len() < PARALLEL_MIN_BYTES {
        1
    } else {
        lane_count(features.len())
    };
    let entries = split_entries_in(features, parts);
    if u32::try_from(entries.len()).is_err() {
        return Err(Error::Unsupported("more than 2^32 feature strings"));
    }
    Ok(entries)
}

fn split_entries_in(features: &[u8], parts: usize) -> Vec<&[u8]> {
    let parts = parts.max(1);
    let chunks = if parts == 1 {
        vec![split_chunk(features, 0, features.len())]
    } else {
        split_chunks_parallel(features, parts)
    };
    merge_chunks(features, chunks)
}

fn split_chunks_parallel(features: &[u8], parts: usize) -> Vec<Chunk<'_>> {
    let width = features.len() / parts;
    thread::scope(|scope| {
        let handles: Vec<_> = (0..parts)
            .map(|part| {
                let begin = part * width;
                let end = if part + 1 == parts {
                    features.len()
                } else {
                    begin + width
                };
                scope.spawn(move || split_chunk(features, begin, end))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

fn split_chunk(features: &[u8], begin: usize, end: usize) -> Chunk<'_> {
    let mut chunk = Chunk {
        first: None,
        last: 0,
        rest: Vec::new(),
    };
    for_each_nul(&features[begin..end], |offset| {
        let at = begin + offset;
        if chunk.first.is_none() {
            chunk.first = Some(at);
        } else {
            chunk.rest.push(&features[chunk.last + 1..at]);
        }
        chunk.last = at;
    });
    chunk
}

fn merge_chunks<'a>(features: &'a [u8], chunks: Vec<Chunk<'a>>) -> Vec<&'a [u8]> {
    let total: usize = chunks
        .iter()
        .map(|chunk| chunk.rest.len() + usize::from(chunk.first.is_some()))
        .sum();
    let mut entries = Vec::with_capacity(total);
    let mut start = 0;
    for chunk in chunks {
        if let Some(first) = chunk.first {
            entries.push(&features[start..first]);
            entries.extend(chunk.rest);
            start = chunk.last + 1;
        }
    }
    entries
}

fn for_each_nul(bytes: &[u8], mut found: impl FnMut(usize)) {
    let scanned = scan_blocks(bytes, &mut found);
    for (offset, &byte) in bytes.iter().enumerate().skip(scanned) {
        if byte == 0 {
            found(offset);
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn scan_blocks(bytes: &[u8], found: &mut impl FnMut(usize)) -> usize {
    use std::arch::x86_64::{__m128i, _mm_cmpeq_epi8, _mm_movemask_epi8, _mm_setzero_si128};
    use std::ptr::read_unaligned;

    const BLOCK: usize = 16;
    let zero = unsafe { _mm_setzero_si128() };
    let mut at = 0;
    while at + BLOCK <= bytes.len() {
        let mut mask = unsafe {
            let block = read_unaligned(bytes.as_ptr().add(at).cast::<__m128i>());
            _mm_movemask_epi8(_mm_cmpeq_epi8(block, zero)) as u32
        };
        while mask != 0 {
            found(at + mask.trailing_zeros() as usize);
            mask &= mask - 1;
        }
        at += BLOCK;
    }
    at
}

#[cfg(not(target_arch = "x86_64"))]
fn scan_blocks(_bytes: &[u8], _found: &mut impl FnMut(usize)) -> usize {
    0
}
