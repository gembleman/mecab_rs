use std::ops::Range;

use super::super::chars;
use super::super::{BLOB_SYNC_EVERY, Layout, NumSpec};
use super::parallel::{BitSink, fill_bits, part_bounds, sync_parts, thread_count};
use super::read_range;
use crate::Error;
use crate::packed::reader::Source;
use crate::wire::u32_values;

const READ_CHUNK: usize = 1 << 16;
const BOUNDARY_SCAN_BYTES: usize = 4096;
const PARALLEL_MIN_BYTES: usize = 1 << 22;
const PARALLEL_MIN_BLOB_BYTES: usize = 1 << 20;

pub(super) fn check_strings(
    source: &Source,
    section: usize,
    layout: &Layout,
) -> Result<Vec<u64>, Error> {
    let char_count = layout.counts.chars as usize;
    let length = layout.counts.strings as usize;
    let base = layout.strings.start;
    let mut words = vec![0u64; length / 64 + 2];
    if length == 0 {
        return Ok(words);
    }
    let threads = thread_count(length >= PARALLEL_MIN_BYTES);
    let mut bounds = Vec::with_capacity(threads + 1);
    bounds.push(0);
    for part in 1..threads {
        bounds.push(next_boundary(
            source,
            section,
            base,
            length / threads * part,
            length,
        )?);
    }
    bounds.push(length);
    fill_bits(&mut words, &bounds, |_, range, sink| {
        check_string_range(source, section, base, range, char_count, sink)
    })?;
    Ok(words)
}

enum Entry {
    Complete(usize),
    Short,
    Invalid,
}

fn parse_blob_entry(bytes: &[u8], layout: &Layout, id_limit_bits: u32) -> Entry {
    let counts = layout.counts;
    let id_bytes = counts.id_bytes as usize;
    let Some(id) = bytes.get(..id_bytes) else {
        return Entry::Short;
    };
    let id = id
        .iter()
        .rev()
        .fold(0u64, |value, &byte| value << 8 | u64::from(byte));
    if id >> id_limit_bits != 0 {
        return Entry::Invalid;
    }
    let mut at = id_bytes;
    for _ in 0..counts.blob_fields {
        loop {
            match bytes.get(at) {
                None => return Entry::Short,
                Some(0) => {
                    at += 1;
                    break;
                }
                Some(&first) if first >= chars::FIRST_LEAD && at + 1 >= bytes.len() => {
                    return Entry::Short;
                }
                Some(_) => match chars::rank_at(bytes, at) {
                    Some((rank, next)) if rank < counts.chars as usize => at = next,
                    _ => return Entry::Invalid,
                },
            }
        }
    }
    Entry::Complete(at)
}

pub(super) fn check_blob(
    source: &Source,
    section: usize,
    layout: &Layout,
    specs: &[NumSpec],
) -> Result<Vec<u64>, Error> {
    let length = layout.counts.blob_bytes as usize;
    let entry_count = layout.counts.blob_entries as usize;
    let mut words = vec![0u64; length / 64 + 2];
    if length == 0 {
        return Ok(words);
    }
    let invalid = || source.broken("the lemma table of the features is not valid");
    let sync: Vec<usize> = u32_values(&read_range(source, section, &layout.blob_sync)?)
        .map(|word| word as usize)
        .collect();
    if sync.len() != entry_count.div_ceil(BLOB_SYNC_EVERY)
        || sync.first() != Some(&0)
        || sync.windows(2).any(|pair| pair[0] >= pair[1])
        || sync.last().is_some_and(|&last| last >= length)
    {
        return Err(invalid());
    }
    let id_limit_bits = specs
        .get(layout.counts.blob_number as usize)
        .ok_or_else(invalid)?
        .hi_bits;
    let threads = thread_count(length >= PARALLEL_MIN_BLOB_BYTES).min(sync.len());
    let parts = sync_parts(sync.len(), threads);
    let bounds = part_bounds(&parts, &sync, length);
    let interval_end = |index: usize| sync.get(index + 1).copied().unwrap_or(length);
    let interval_entries = |index: usize| {
        if index + 1 == sync.len() {
            entry_count - index * BLOB_SYNC_EVERY
        } else {
            BLOB_SYNC_EVERY
        }
    };
    fill_bits(&mut words, &bounds, |number, _, sink| {
        let (first, last) = parts[number];
        (first..last).try_for_each(|index| {
            check_blob_range(
                source,
                section,
                layout,
                id_limit_bits,
                sync[index]..interval_end(index),
                interval_entries(index),
                sink,
            )
        })
    })?;
    Ok(words)
}

fn check_blob_range(
    source: &Source,
    section: usize,
    layout: &Layout,
    id_limit_bits: u32,
    range: Range<usize>,
    expected: usize,
    sink: &mut BitSink,
) -> Result<(), Error> {
    let invalid = || source.broken("the lemma table of the features is not valid");
    let base = layout.blob.start;
    let mut buffer: Vec<u8> = Vec::new();
    let mut carry = 0;
    let mut done = range.start;
    let mut entries = 0;
    while done < range.end {
        let part = READ_CHUNK.min(range.end - done);
        buffer.resize(carry + part, 0);
        source.read(section, base + done, &mut buffer[carry..])?;
        let origin = done - carry;
        let mut at = 0;
        while at < buffer.len() {
            if entries == expected {
                return Err(invalid());
            }
            match parse_blob_entry(&buffer[at..], layout, id_limit_bits) {
                Entry::Complete(size) => {
                    sink.set(origin + at, true);
                    entries += 1;
                    at += size;
                }
                Entry::Short => break,
                Entry::Invalid => return Err(invalid()),
            }
        }
        buffer.copy_within(at.., 0);
        carry = buffer.len() - at;
        done += part;
    }
    if carry != 0 || entries != expected {
        return Err(invalid());
    }
    Ok(())
}

fn next_boundary(
    source: &Source,
    section: usize,
    base: usize,
    from: usize,
    length: usize,
) -> Result<usize, Error> {
    let mut buffer = vec![0u8; BOUNDARY_SCAN_BYTES];
    let mut at = from.saturating_sub(1);
    while at < length {
        let part = buffer.len().min(length - at);
        source.read(section, base + at, &mut buffer[..part])?;
        if let Some(found) = buffer[..part].iter().position(|&byte| byte == 0) {
            return Ok(at + found + 1);
        }
        at += part;
    }
    Ok(length)
}

fn check_string_range(
    source: &Source,
    section: usize,
    base: usize,
    range: Range<usize>,
    char_count: usize,
    sink: &mut BitSink,
) -> Result<(), Error> {
    let mut buffer = vec![0u8; READ_CHUNK.min(range.len())];
    let mut carry = 0;
    let mut done = range.start;
    while done < range.end {
        let part = READ_CHUNK.min(range.end - done);
        if buffer.len() < carry + part {
            buffer.resize(carry + part, 0);
        }
        source.read(section, base + done, &mut buffer[carry..carry + part])?;
        let filled = carry + part;
        let origin = done - carry;
        let cut = buffer[..filled]
            .iter()
            .rposition(|&byte| byte == 0)
            .map_or(0, |at| at + 1);
        let mut at = 0;
        while at < cut {
            let next = if buffer[at] == 0 {
                Some(at + 1)
            } else {
                chars::rank_at(&buffer[..cut], at)
                    .filter(|&(rank, _)| rank < char_count)
                    .map(|(_, next)| next)
            };
            let Some(next) = next else {
                return Err(source.broken("the feature strings are not valid codes"));
            };
            sink.set(origin + at, true);
            at = next;
        }
        buffer.copy_within(cut..filled, 0);
        carry = filled - cut;
        done += part;
    }
    if carry != 0 {
        return Err(source.broken("the feature strings do not end with a NUL byte"));
    }
    Ok(())
}
