use std::ops::Range;

use super::super::chars;
use super::super::{
    Layout, NumSpec, OP_BLOB, OP_INLINE, OP_NUM, OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF,
    RECORD_PADDING, SYNC_EVERY, find_byte, read_bits,
};
use super::parallel::{BitSink, fill_bits, part_bounds, sync_parts, thread_count};
use super::{Tables, read_range};
use crate::Error;
use crate::packed::reader::Source;
use crate::wire::u32_values;

const RECORD_CHUNK: usize = 1 << 16;
const INITIAL_RECORD_WINDOW: usize = 128;
const PARALLEL_MIN_RECORDS: usize = 1 << 15;

pub(super) fn check_records(
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
    let parts = sync_parts(sync.len(), threads);
    let bounds = part_bounds(&parts, &sync, records_length);
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

struct RecordCursor<'a> {
    record: &'a [u8],
    bit: usize,
}

impl<'a> RecordCursor<'a> {
    #[inline(always)]
    fn new(record: &'a [u8]) -> Self {
        Self { record, bit: 0 }
    }

    #[inline(always)]
    fn take(&mut self, width: u32) -> Result<u64, Fail> {
        if ((self.bit + width as usize) >> 3) + 8 > self.record.len() {
            return Err(Fail::Short);
        }
        let value = read_bits(self.record, self.bit, width);
        self.bit += width as usize;
        Ok(value)
    }

    #[inline(always)]
    fn skip(&mut self, width: u32) {
        self.bit += width as usize;
    }

    #[inline(always)]
    fn check_ref(
        &mut self,
        width: u32,
        limit: usize,
        starts: &[u64],
        reason: &'static str,
    ) -> Result<(), Fail> {
        let target = self.take(width)? as usize;
        if target >= limit || starts[target >> 6] >> (target & 63) & 1 == 0 {
            return Err(Fail::Invalid(reason));
        }
        Ok(())
    }

    #[inline(always)]
    fn check_number_part(&mut self, spec: &NumSpec) -> Result<(), Fail> {
        let index = self.take(spec.lo_bits)?;
        if index >= u64::from(spec.lo_count) {
            return Err(Fail::Invalid(
                "a feature record names a number part that does not exist",
            ));
        }
        Ok(())
    }

    #[inline(always)]
    fn check_inline(&mut self, chars: usize) -> Result<(), Fail> {
        let start = self.bit.div_ceil(8);
        let rest = self.record.get(start..).ok_or(Fail::Short)?;
        let length = find_byte(rest, 0);
        if length == rest.len() {
            return Err(Fail::Short);
        }
        if !chars::are_codes(&rest[..length], chars) {
            return Err(Fail::Invalid(
                "a feature record has text that is not valid codes",
            ));
        }
        self.bit = (start + length + 1) * 8;
        Ok(())
    }

    #[inline(always)]
    fn check_op(&mut self, op: u8, tables: &Tables) -> Result<(), Fail> {
        match op {
            OP_REF => self.check_ref(
                tables.ref_bits,
                tables.strings,
                &tables.string_boundaries,
                "a feature record names a string that does not exist",
            ),
            OP_BLOB => self.check_ref(
                tables.blob_bits,
                tables.blob_bytes,
                &tables.blob_starts,
                "a feature record names a lemma that does not exist",
            ),
            OP_NUMB..=OP_NUMB_MAX => {
                self.check_number_part(&tables.specs[usize::from(op - OP_NUMB)])
            }
            OP_INLINE => self.check_inline(tables.chars),
            OP_NUM..=OP_NUM_MAX => {
                let spec = &tables.specs[usize::from(op - OP_NUM)];
                self.skip(spec.hi_bits);
                self.check_number_part(spec)
            }
            _ => Ok(()),
        }
    }
}

fn check_names<'t>(
    shape: usize,
    template: usize,
    tables: &'t Tables<'_>,
) -> Result<&'t [u8], Fail> {
    let Some(shape_ops) = tables
        .shape_ops
        .get(shape)
        .filter(|_| template < tables.templates)
    else {
        return Err(Fail::Invalid(
            "a feature record names a template or a shape that does not exist",
        ));
    };
    if shape_ops.len() >= tables.positions {
        return Err(Fail::Invalid(
            "a feature record joins a template and a shape that do not fit",
        ));
    }
    Ok(shape_ops)
}

fn parse_record(record: &[u8], tables: &Tables) -> Result<usize, Fail> {
    let mut cursor = RecordCursor::new(record);
    let template = cursor.take(tables.template_bits)? as usize;
    let shape = cursor.take(tables.shape_bits)? as usize;
    for &op in check_names(shape, template, tables)? {
        cursor.check_op(op, tables)?;
    }
    let length = cursor.bit.div_ceil(8).max(1);
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
