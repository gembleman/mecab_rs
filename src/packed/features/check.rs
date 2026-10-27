use std::ops::Range;
use std::thread;

use super::{
    HEADER_BYTES, Layout, MAX_NUMERIC, MAX_SLOTS, NumSpec, OP_COPY, OP_COPY_MAX, OP_EMPTY, OP_HIGH,
    OP_HIGH_MAX, OP_INLINE, OP_KEY, OP_NUM, OP_NUM_MAX, OP_REF, OP_STAR, RECORD_PADDING,
    SPEC_BYTES, SYNC_EVERY, find_byte, read_bits,
};
use crate::Error;
use crate::packed::reader::Source;

const READ_CHUNK: usize = 1 << 16;
const RECORD_CHUNK: usize = 1 << 18;
const BOUNDARY_SCAN_BYTES: usize = 4096;
const INITIAL_RECORD_WINDOW: usize = 128;
const PARALLEL_MIN_BYTES: usize = 1 << 22;
const PARALLEL_MIN_RECORDS: usize = 1 << 15;
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

fn parallel<T: Send>(count: usize, work: impl Fn(usize) -> T + Sync) -> Vec<T> {
    if count == 1 {
        return vec![work(0)];
    }
    thread::scope(|scope| {
        let handles: Vec<_> = (0..count)
            .map(|number| {
                let work = &work;
                scope.spawn(move || work(number))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a check thread panicked"))
            .collect()
    })
}

fn offsets_of(bytes: &[u8], blob_length: usize) -> Option<Vec<usize>> {
    let offsets: Vec<usize> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&word| u32::from_le_bytes(word) as usize)
        .collect();
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
    holes: Vec<u32>,
    operations: Vec<u32>,
    specs: Vec<NumSpec>,
    string_boundaries: Vec<u64>,
    strings: usize,
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

    let mut template_bytes = read_range(source, section, &layout.templates)?;
    template_bytes.truncate(counts.template_bytes as usize);
    let template_offsets = offsets_of(
        &read_range(source, section, &layout.template_offsets)?,
        template_bytes.len(),
    )
    .ok_or_else(|| invalid("the feature templates are not delimited correctly"))?;
    let mut holes = Vec::with_capacity(template_offsets.len());
    for pair in template_offsets.windows(2) {
        holes.push(
            template_holes(&template_bytes[pair[0]..pair[1]])
                .ok_or_else(|| invalid("a feature template is not valid"))?,
        );
    }
    drop(template_bytes);

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
        for &op in ops {
            match op {
                OP_REF | OP_INLINE => slots = (slots + 1).min(MAX_SLOTS),
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

    let string_boundaries = check_strings(source, section, &layout)?;
    let tables = Tables {
        holes,
        operations,
        specs: specs.clone(),
        string_boundaries,
        strings: counts.strings as usize,
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

fn template_holes(program: &[u8]) -> Option<u32> {
    let mut at = 0;
    let mut pieces = 0u32;
    while at < program.len() {
        let (start, length) = if program[at] < 255 {
            (at + 1, usize::from(program[at]))
        } else {
            let bytes = program.get(at + 1..at + 5)?;
            (at + 5, u32::from_le_bytes(bytes.try_into().ok()?) as usize)
        };
        let piece = program.get(start..start.checked_add(length)?)?;
        str::from_utf8(piece).ok()?;
        at = start + length;
        pieces += 1;
    }
    pieces.checked_sub(1)
}

fn set_bit(words: &mut [u64], position: usize, condition: bool) {
    words[position >> 6] |= u64::from(condition) << (position & 63);
}

fn check_strings(source: &Source, section: usize, layout: &Layout) -> Result<Vec<u64>, Error> {
    let length = layout.counts.strings as usize;
    let base = layout.strings.start;
    let mut words = vec![0u64; length / 64 + 2];
    if length == 0 {
        return Ok(words);
    }
    let threads = thread_count(length >= PARALLEL_MIN_BYTES);
    let mut bounds = vec![0];
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
    let parts = parallel(threads, |number| {
        let range = bounds[number]..bounds[number + 1];
        if range.is_empty() {
            return Ok((0, Vec::new()));
        }
        check_string_range(source, section, base, range)
    });
    for part in parts {
        let (first_word, local) = part?;
        for (offset, word) in local.into_iter().enumerate() {
            words[first_word + offset] |= word;
        }
    }
    Ok(words)
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
) -> Result<(usize, Vec<u64>), Error> {
    let first_word = range.start >> 6;
    let mut words = vec![0u64; (range.end >> 6) + 1 - first_word];
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
        if str::from_utf8(&buffer[..cut]).is_err() {
            return Err(source.broken("the feature strings are not valid UTF-8"));
        }
        for (at, &byte) in buffer[..cut].iter().enumerate() {
            let position = origin + at;
            set_bit(
                &mut words,
                position - (first_word << 6),
                byte.cast_signed() >= -64,
            );
        }
        buffer.copy_within(cut..filled, 0);
        carry = filled - cut;
        done += part;
    }
    if carry != 0 {
        return Err(source.broken("the feature strings do not end with a NUL byte"));
    }
    Ok((first_word, words))
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
    let sync: Vec<usize> = read_range(source, section, &layout.sync)?
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&word| u32::from_le_bytes(word) as usize)
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
    let results = parallel(parts.len(), |number| {
        let (first, last) = parts[number];
        let begin = sync[first];
        let end = sync.get(last).copied().unwrap_or(records_length);
        let expected = if last == sync.len() {
            record_count - first * SYNC_EVERY
        } else {
            (last - first) * SYNC_EVERY
        };
        walk_records(source, section, layout, tables, begin..end, expected)
    });
    let mut starts = vec![0u64; records_length / 64 + 2];
    for result in results {
        let (first_word, local) = result?;
        for (offset, word) in local.into_iter().enumerate() {
            starts[first_word + offset] |= word;
        }
    }
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
    let (Some(&holes), Some(&operations)) =
        (tables.holes.get(template), tables.operations.get(shape))
    else {
        return Err(Fail::Invalid(
            "a feature record names a template or a shape that does not exist",
        ));
    };
    if holes != operations {
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
            OP_INLINE => {
                let start = bit.div_ceil(8);
                let rest = record.get(start..).ok_or(Fail::Short)?;
                let length = find_byte(rest, 0);
                if length == rest.len() {
                    return Err(Fail::Short);
                }
                if str::from_utf8(&rest[..length]).is_err() {
                    return Err(Fail::Invalid("a feature record has text that is not UTF-8"));
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
) -> Result<(usize, Vec<u64>), Error> {
    let invalid = |reason: &'static str| source.broken(reason);
    let total = layout.counts.records as usize + RECORD_PADDING;
    let base = layout.records.start;
    let first_word = range.start >> 6;
    let mut words = vec![0u64; (range.end >> 6) + 1 - first_word];
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
        set_bit(&mut words, at - (first_word << 6), true);
        at += length;
    }
    if at != range.end {
        return Err(invalid(
            "the feature records do not end where the sync table says",
        ));
    }
    Ok((first_word, words))
}
