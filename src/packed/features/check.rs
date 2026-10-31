use std::ops::Range;
use std::thread;

use super::chars::{self, ENTRY_BYTES};
use super::pieces::{POSITION_BYTES, Position};
use super::{
    BLOB_SYNC_EVERY, HEADER_BYTES, Layout, MAX_NUMERIC, MAX_RULE_SUFFIX, MAX_SLOTS, NumSpec,
    OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_COPY_MAX, OP_DERIVE, OP_EMPTY, OP_HIGH, OP_HIGH_MAX,
    OP_INLINE, OP_KEY, OP_NUM, OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF, OP_STAR, RECORD_PADDING,
    RULE_BYTES, RULE_CHAR_BITS, RULE_CHAR_MASK, RULE_HEADER_BYTES, SPEC_BYTES, SYNC_EVERY,
    find_byte, read_bits,
};
use crate::Error;
use crate::packed::reader::Source;
use crate::wire::u32_values;

const READ_CHUNK: usize = 1 << 16;
const RECORD_CHUNK: usize = 1 << 16;
const BOUNDARY_SCAN_BYTES: usize = 4096;
const INITIAL_RECORD_WINDOW: usize = 128;
const PARALLEL_MIN_BYTES: usize = 1 << 22;
const PARALLEL_MIN_RECORDS: usize = 1 << 15;
const PARALLEL_MIN_BLOB_BYTES: usize = 1 << 20;
const MAX_THREADS: usize = 8;

pub(in crate::packed) struct Checked {
    pub(in crate::packed) layout: Layout,
    pub(in crate::packed) specs: Vec<NumSpec>,
    starts: Vec<u64>,
    records: usize,
}

impl Checked {
    #[inline]
    pub(in crate::packed) fn is_record(&self, offset: usize) -> bool {
        offset < self.records
            && self
                .starts
                .get(offset >> 6)
                .is_some_and(|word| word >> (offset & 63) & 1 != 0)
    }
}

fn thread_count(wanted: bool) -> usize {
    if wanted {
        thread::available_parallelism().map_or(1, |count| count.get().min(MAX_THREADS))
    } else {
        1
    }
}

struct BitSink<'a> {
    words: &'a mut [u64],
    first_word: usize,
    tail: u64,
}

impl BitSink<'_> {
    #[inline]
    fn set(&mut self, position: usize, condition: bool) {
        let bit = u64::from(condition) << (position & 63);
        match self.words.get_mut((position >> 6) - self.first_word) {
            Some(word) => *word |= bit,
            None => self.tail |= bit,
        }
    }
}

fn fill_bits(
    words: &mut [u64],
    bounds: &[usize],
    work: impl Fn(usize, Range<usize>, &mut BitSink) -> Result<(), Error> + Sync,
) -> Result<(), Error> {
    let parts = bounds.len() - 1;
    let mut sinks = Vec::with_capacity(parts);
    let mut rest = &mut *words;
    for (number, pair) in bounds.windows(2).enumerate() {
        let first_word = pair[0] >> 6;
        let owned = if number + 1 == parts {
            rest.len()
        } else {
            (pair[1] >> 6) - first_word
        };
        let (mine, others) = rest.split_at_mut(owned);
        rest = others;
        sinks.push(BitSink {
            words: mine,
            first_word,
            tail: 0,
        });
    }
    let results: Vec<Result<(), Error>> = if parts == 1 {
        vec![work(0, bounds[0]..bounds[1], &mut sinks[0])]
    } else {
        thread::scope(|scope| {
            let handles: Vec<_> = sinks
                .iter_mut()
                .zip(bounds.windows(2))
                .enumerate()
                .map(|(number, (sink, pair))| {
                    let work = &work;
                    let range = pair[0]..pair[1];
                    scope.spawn(move || work(number, range, sink))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a check thread panicked"))
                .collect()
        })
    };
    let tails: Vec<u64> = sinks.iter().map(|sink| sink.tail).collect();
    drop(sinks);
    for result in results {
        result?;
    }
    for (number, tail) in tails.into_iter().enumerate().take(parts - 1) {
        words[bounds[number + 1] >> 6] |= tail;
    }
    Ok(())
}

fn offsets_of(bytes: &[u8], blob_length: usize) -> Option<Vec<usize>> {
    let offsets: Vec<usize> = u32_values(bytes).map(|word| word as usize).collect();
    let valid = offsets.first() == Some(&0)
        && offsets.last() == Some(&blob_length)
        && offsets.windows(2).all(|pair| pair[0] <= pair[1]);
    valid.then_some(offsets)
}

fn read_range(source: &Source, section: usize, range: &Range<usize>) -> Result<Vec<u8>, Error> {
    let mut bytes = vec![0; range.len()];
    source.read(section, range.start, &mut bytes)?;
    Ok(bytes)
}

struct Tables {
    templates: usize,
    positions: usize,
    operations: Vec<u32>,
    specs: Vec<NumSpec>,
    string_boundaries: Vec<u64>,
    blob_starts: Vec<u64>,
    blob_bytes: usize,
    blob_bits: u32,
    strings: usize,
    chars: usize,
    ref_bits: u32,
    template_bits: u32,
    shape_bits: u32,
    shape_ops: Vec<Vec<u8>>,
}

pub(in crate::packed) fn check(source: &Source, section: usize) -> Result<Checked, Error> {
    let length = source.section_length(section);
    if length < HEADER_BYTES {
        return Err(source.broken("the feature section is shorter than its header"));
    }
    let mut header = [0u8; HEADER_BYTES];
    source.read(section, 0, &mut header)?;
    let layout = Layout::parse(&header, length)
        .ok_or_else(|| source.broken("the feature section header is not valid"))?;
    let counts = layout.counts;
    let invalid = |reason: &'static str| source.broken(reason);

    let spec_bytes = read_range(source, section, &layout.specs)?;
    let specs: Vec<NumSpec> = spec_bytes
        .as_chunks::<SPEC_BYTES>()
        .0
        .iter()
        .map(|bytes| NumSpec::parse(bytes, counts.lo_total))
        .collect::<Option<_>>()
        .ok_or_else(|| invalid("a number of the features is not specified correctly"))?;

    let blob_is_valid = counts.blob_fields == 0
        || specs
            .get(counts.blob_number as usize)
            .is_some_and(|spec| spec.hi_bits >= 1 && counts.id_bytes == spec.hi_bits.div_ceil(8));
    if !blob_is_valid {
        return Err(invalid("the lemma table of the features is not valid"));
    }

    let table = read_range(source, section, &layout.chars)?;
    let table_is_valid = table
        .as_chunks::<ENTRY_BYTES>()
        .0
        .iter()
        .all(|&entry| chars::entry_is_char(u64::from_le_bytes(entry)));
    if !table_is_valid {
        return Err(invalid("the character table of the features is not valid"));
    }

    check_pieces(source, section, &layout)?;

    let shape_bytes = read_range(source, section, &layout.shapes)?;
    let shape_offsets = offsets_of(
        &read_range(source, section, &layout.shape_offsets)?,
        shape_bytes.len(),
    )
    .ok_or_else(|| invalid("the feature shapes are not delimited correctly"))?;
    let mut operations = Vec::with_capacity(shape_offsets.len());
    let mut shape_ops = Vec::with_capacity(shape_offsets.len());
    for pair in shape_offsets.windows(2) {
        let ops = &shape_bytes[pair[0]..pair[1]];
        let mut slots = 0usize;
        let mut numbers = 0u8;
        let mut blob_seen = 0u32;
        for &op in ops {
            match op {
                OP_REF | OP_INLINE => slots = (slots + 1).min(MAX_SLOTS),
                OP_BLOB if blob_seen == 0 && counts.blob_fields > 0 => {
                    blob_seen = 1;
                    slots = (slots + 1).min(MAX_SLOTS);
                }
                OP_BLOB_NEXT if blob_seen > 0 && blob_seen < counts.blob_fields => {
                    blob_seen += 1;
                    slots = (slots + 1).min(MAX_SLOTS);
                }
                OP_NUMB..=OP_NUMB_MAX
                    if blob_seen > 0 && u32::from(op - OP_NUMB) == counts.blob_number =>
                {
                    numbers |= 1 << (op - OP_NUMB);
                }
                OP_DERIVE if counts.rule_groups > 0 => slots = (slots + 1).min(MAX_SLOTS),
                OP_EMPTY | OP_STAR | OP_KEY => {}
                OP_COPY..=OP_COPY_MAX if usize::from(op - OP_COPY) < slots => {}
                OP_NUM..=OP_NUM_MAX if usize::from(op - OP_NUM) < specs.len() => {
                    numbers |= 1 << (op - OP_NUM);
                }
                OP_HIGH..=OP_HIGH_MAX
                    if usize::from(op - OP_HIGH) < MAX_NUMERIC
                        && numbers >> (op - OP_HIGH) & 1 != 0 => {}
                _ => {
                    return Err(invalid(
                        "a feature shape has an operation that is not valid",
                    ));
                }
            }
        }
        operations.push(ops.len() as u32);
        shape_ops.push(ops.to_vec());
    }
    drop(shape_bytes);

    check_rules(source, section, &layout)?;
    let string_boundaries = check_strings(source, section, &layout)?;
    let blob_starts = check_blob(source, section, &layout, &specs)?;
    let tables = Tables {
        templates: counts.templates as usize,
        positions: counts.positions as usize,
        operations,
        specs: specs.clone(),
        string_boundaries,
        blob_starts,
        blob_bytes: counts.blob_bytes as usize,
        blob_bits: layout.blob_bits,
        strings: counts.strings as usize,
        chars: counts.chars as usize,
        ref_bits: layout.ref_bits,
        template_bits: layout.template_bits,
        shape_bits: layout.shape_bits,
        shape_ops,
    };
    let starts = check_records(source, section, &layout, &tables)?;
    Ok(Checked {
        layout,
        specs,
        starts,
        records: counts.records as usize,
    })
}

fn words_of<const WIDTH: usize>(bytes: &[u8], read: fn([u8; WIDTH]) -> u32) -> Vec<u32> {
    bytes
        .as_chunks::<WIDTH>()
        .0
        .iter()
        .map(|&word| read(word))
        .collect()
}

fn check_rules(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    let counts = layout.counts;
    if counts.rule_groups == 0 {
        return Ok(());
    }
    let invalid = || source.broken("the derivation rules of the features are not valid");
    let groups = counts.rule_groups as usize;
    let entry_count = counts.rule_entries as usize;
    let group_of_template = words_of::<2>(
        &read_range(source, section, &layout.template_groups)?,
        |word| u32::from(u16::from_le_bytes(word)),
    );
    if group_of_template
        .iter()
        .any(|&group| group as usize > groups)
    {
        return Err(invalid());
    }
    let starts = words_of::<4>(
        &read_range(source, section, &layout.group_starts)?,
        u32::from_le_bytes,
    );
    let starts_are_valid = starts.len() == groups + 1
        && starts.first() == Some(&0)
        && starts
            .last()
            .is_some_and(|&last| last as usize == entry_count)
        && starts.windows(2).all(|pair| pair[0] < pair[1]);
    if !starts_are_valid {
        return Err(invalid());
    }
    let entries = words_of::<4>(
        &read_range(source, section, &layout.rule_entries)?,
        u32::from_le_bytes,
    );
    let rule_count = counts.rule_count;
    for pair in starts.windows(2) {
        let group = &entries[pair[0] as usize..pair[1] as usize];
        let entry_is_valid = |&entry: &u32| {
            char::from_u32(entry & RULE_CHAR_MASK).is_some() && entry >> RULE_CHAR_BITS < rule_count
        };
        let ascending = group
            .windows(2)
            .all(|pair| pair[0] & RULE_CHAR_MASK < pair[1] & RULE_CHAR_MASK);
        if !group.iter().all(entry_is_valid) || !ascending {
            return Err(invalid());
        }
    }
    let rules = read_range(source, section, &layout.rules)?;
    let rules = &rules[..RULE_BYTES * counts.rule_count as usize];
    for rule in rules.as_chunks::<RULE_BYTES>().0 {
        let length = usize::from(rule[1]);
        let suffix_end = RULE_HEADER_BYTES + length;
        let is_valid = length <= MAX_RULE_SUFFIX
            && rule[suffix_end..].iter().all(|&byte| byte == 0)
            && str::from_utf8(&rule[RULE_HEADER_BYTES..suffix_end]).is_ok();
        if !is_valid {
            return Err(invalid());
        }
    }
    Ok(())
}

fn check_pieces(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    let counts = layout.counts;
    let invalid = |reason: &'static str| source.broken(reason);
    let position_bytes = read_range(source, section, &layout.positions)?;
    let entry_bits = counts.entry_bits as usize;
    let positions: Vec<Position> = position_bytes
        .as_chunks::<POSITION_BYTES>()
        .0
        .iter()
        .map(Position::parse)
        .collect::<Option<_>>()
        .ok_or_else(|| invalid("a piece position of the features is not valid"))?;
    if positions.iter().any(|position| {
        !position.constant && position.shift as usize + position.bits as usize > entry_bits
    }) {
        return Err(invalid("a piece position of the features leaves its entry"));
    }
    let mut pool = read_range(source, section, &layout.pool)?;
    pool.truncate(counts.pool_bytes as usize);
    let offsets = offsets_of(
        &read_range(source, section, &layout.piece_offsets)?,
        pool.len(),
    )
    .ok_or_else(|| invalid("the feature pieces are not delimited correctly"))?;
    for pair in offsets.windows(2) {
        if str::from_utf8(&pool[pair[0]..pair[1]]).is_err() {
            return Err(invalid("a feature piece is not valid UTF-8"));
        }
    }
    for position in positions.iter().filter(|position| position.constant) {
        let start = position.base as usize;
        let piece = pool.get(start..start + position.shift as usize);
        if piece.is_none_or(|piece| str::from_utf8(piece).is_err()) {
            return Err(invalid("a constant feature piece is not valid"));
        }
    }
    drop(pool);
    let entries = read_range(source, section, &layout.entries)?;
    let pieces = u64::from(counts.pieces);
    for template in 0..counts.templates as usize {
        for position in positions.iter().filter(|position| !position.constant) {
            let local = read_bits(
                &entries,
                template * entry_bits + position.shift as usize,
                position.bits,
            );
            if u64::from(position.base) + local >= pieces {
                return Err(invalid(
                    "a feature template names a piece that does not exist",
                ));
            }
        }
    }
    Ok(())
}

fn check_strings(source: &Source, section: usize, layout: &Layout) -> Result<Vec<u64>, Error> {
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

fn check_blob(
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
    let id_limit_bits = specs[layout.counts.blob_number as usize].hi_bits;
    let threads = thread_count(length >= PARALLEL_MIN_BLOB_BYTES).min(sync.len());
    let per_part = sync.len().div_ceil(threads);
    let parts: Vec<(usize, usize)> = (0..threads)
        .map(|number| (number * per_part, ((number + 1) * per_part).min(sync.len())))
        .filter(|&(first, last)| first < last)
        .collect();
    let mut bounds: Vec<usize> = parts.iter().map(|&(first, _)| sync[first]).collect();
    bounds.push(length);
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

fn check_records(
    source: &Source,
    section: usize,
    layout: &Layout,
    tables: &Tables,
) -> Result<Vec<u64>, Error> {
    let counts = layout.counts;
    let records_length = counts.records as usize;
    let record_count = counts.record_count as usize;
    let invalid = |reason: &'static str| source.broken(reason);
    let sync: Vec<usize> = u32_values(&read_range(source, section, &layout.sync)?)
        .map(|word| word as usize)
        .collect();
    if sync.len() != record_count.div_ceil(SYNC_EVERY)
        || sync.first().is_some_and(|&first| first != 0)
        || sync.windows(2).any(|pair| pair[0] >= pair[1])
        || sync.last().is_some_and(|&last| last >= records_length)
    {
        return Err(invalid(
            "the sync table of the feature records is not valid",
        ));
    }
    if record_count == 0 {
        return if records_length == 0 {
            Ok(Vec::new())
        } else {
            Err(invalid("there are feature records without records"))
        };
    }
    let threads = thread_count(record_count >= PARALLEL_MIN_RECORDS).min(sync.len());
    let per_part = sync.len().div_ceil(threads);
    let parts: Vec<(usize, usize)> = (0..threads)
        .map(|number| (number * per_part, ((number + 1) * per_part).min(sync.len())))
        .filter(|&(first, last)| first < last)
        .collect();
    let mut bounds: Vec<usize> = parts.iter().map(|&(first, _)| sync[first]).collect();
    bounds.push(records_length);
    let mut starts = vec![0u64; records_length / 64 + 2];
    fill_bits(&mut starts, &bounds, |number, range, sink| {
        let (first, last) = parts[number];
        let expected = if last == sync.len() {
            record_count - first * SYNC_EVERY
        } else {
            (last - first) * SYNC_EVERY
        };
        walk_records(source, section, layout, tables, range, expected, sink)
    })?;
    Ok(starts)
}

#[derive(Default)]
struct Window {
    bytes: Vec<u8>,
    start: usize,
}

impl Window {
    fn ensure(
        &mut self,
        source: &Source,
        section: usize,
        at: usize,
        available: usize,
        need: usize,
    ) -> Result<(), Error> {
        if at >= self.start && at + need <= self.start + self.bytes.len() {
            return Ok(());
        }
        if need > available {
            return Err(source.broken("a feature record reaches beyond the records"));
        }
        let size = need.max(RECORD_CHUNK).min(available);
        self.bytes.resize(size, 0);
        source.read(section, at, &mut self.bytes)?;
        self.start = at;
        Ok(())
    }

    fn from(&self, at: usize) -> &[u8] {
        &self.bytes[at - self.start..]
    }
}

enum Fail {
    Short,
    Invalid(&'static str),
}

fn parse_record(record: &[u8], tables: &Tables) -> Result<usize, Fail> {
    let read = |bit: usize, width: u32| {
        if ((bit + width as usize) >> 3) + 8 > record.len() {
            Err(Fail::Short)
        } else {
            Ok(read_bits(record, bit, width))
        }
    };
    let mut bit = 0usize;
    let template = read(bit, tables.template_bits)? as usize;
    bit += tables.template_bits as usize;
    let shape = read(bit, tables.shape_bits)? as usize;
    bit += tables.shape_bits as usize;
    let Some(&operations) = tables
        .operations
        .get(shape)
        .filter(|_| template < tables.templates)
    else {
        return Err(Fail::Invalid(
            "a feature record names a template or a shape that does not exist",
        ));
    };
    if operations as usize >= tables.positions {
        return Err(Fail::Invalid(
            "a feature record joins a template and a shape that do not fit",
        ));
    }
    for &op in &tables.shape_ops[shape] {
        match op {
            OP_REF => {
                let target = read(bit, tables.ref_bits)? as usize;
                bit += tables.ref_bits as usize;
                if target >= tables.strings
                    || tables.string_boundaries[target >> 6] >> (target & 63) & 1 == 0
                {
                    return Err(Fail::Invalid(
                        "a feature record names a string that does not exist",
                    ));
                }
            }
            OP_BLOB => {
                let target = read(bit, tables.blob_bits)? as usize;
                bit += tables.blob_bits as usize;
                if target >= tables.blob_bytes
                    || tables.blob_starts[target >> 6] >> (target & 63) & 1 == 0
                {
                    return Err(Fail::Invalid(
                        "a feature record names a lemma that does not exist",
                    ));
                }
            }
            OP_NUMB..=OP_NUMB_MAX => {
                let spec = &tables.specs[usize::from(op - OP_NUMB)];
                let index = read(bit, spec.lo_bits)?;
                bit += spec.lo_bits as usize;
                if index >= u64::from(spec.lo_count) {
                    return Err(Fail::Invalid(
                        "a feature record names a number part that does not exist",
                    ));
                }
            }
            OP_INLINE => {
                let start = bit.div_ceil(8);
                let rest = record.get(start..).ok_or(Fail::Short)?;
                let length = find_byte(rest, 0);
                if length == rest.len() {
                    return Err(Fail::Short);
                }
                if !chars::are_codes(&rest[..length], tables.chars) {
                    return Err(Fail::Invalid(
                        "a feature record has text that is not valid codes",
                    ));
                }
                bit = (start + length + 1) * 8;
            }
            OP_NUM..=OP_NUM_MAX => {
                let spec = &tables.specs[usize::from(op - OP_NUM)];
                bit += spec.hi_bits as usize;
                let index = read(bit, spec.lo_bits)?;
                bit += spec.lo_bits as usize;
                if index >= u64::from(spec.lo_count) {
                    return Err(Fail::Invalid(
                        "a feature record names a number part that does not exist",
                    ));
                }
            }
            _ => {}
        }
    }
    let length = bit.div_ceil(8).max(1);
    if length + RECORD_PADDING > record.len() {
        return Err(Fail::Short);
    }
    Ok(length)
}

fn walk_records(
    source: &Source,
    section: usize,
    layout: &Layout,
    tables: &Tables,
    range: Range<usize>,
    expected: usize,
    sink: &mut BitSink,
) -> Result<(), Error> {
    let invalid = |reason: &'static str| source.broken(reason);
    let total = layout.counts.records as usize + RECORD_PADDING;
    let base = layout.records.start;
    let mut window = Window::default();
    let mut at = range.start;
    for _ in 0..expected {
        let mut need = INITIAL_RECORD_WINDOW;
        let length = loop {
            let available = total - at;
            window.ensure(source, section, base + at, available, need.min(available))?;
            match parse_record(window.from(base + at), tables) {
                Ok(length) => break length,
                Err(Fail::Short) if need < available => need *= 4,
                Err(Fail::Short) => {
                    return Err(invalid("a feature record reaches beyond the records"));
                }
                Err(Fail::Invalid(reason)) => return Err(invalid(reason)),
            }
        };
        if at + length > range.end {
            return Err(invalid("a feature record reaches beyond its part"));
        }
        sink.set(at, true);
        at += length;
    }
    if at != range.end {
        return Err(invalid(
            "the feature records do not end where the sync table says",
        ));
    }
    Ok(())
}
