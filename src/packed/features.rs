
pub(super) mod check;
pub(super) mod encode;

use std::fmt;
use std::ops::Range;

pub(super) const OP_REF: u8 = 0x00;
pub(super) const OP_EMPTY: u8 = 0x01;
pub(super) const OP_STAR: u8 = 0x02;
pub(super) const OP_INLINE: u8 = 0x03;
pub(super) const OP_COPY: u8 = 0x10;
pub(super) const OP_NUM: u8 = 0x20;
pub(super) const OP_HIGH: u8 = 0x30;
pub(super) const MAX_SLOTS: usize = 16;
pub(super) const MAX_NUMERIC: usize = 4;
pub(super) const SPEC_BYTES: usize = 16;
pub(super) const HEADER_BYTES: usize = 64;
pub(super) const SYNC_EVERY: usize = 4096;
pub(super) const RECORD_PADDING: usize = 16;
pub(super) const SLACK: usize = 16;

pub(super) fn bits_for(count: u64) -> u32 {
    if count <= 1 {
        0
    } else {
        64 - (count - 1).leading_zeros()
    }
}

fn align8(value: u64) -> Option<u64> {
    value.checked_add(7).map(|sum| sum & !7)
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Counts {
    pub(super) templates: u32,
    pub(super) shapes: u32,
    pub(super) numerics: u32,
    pub(super) lo_total: u32,
    pub(super) template_bytes: u64,
    pub(super) shape_bytes: u64,
    pub(super) strings: u64,
    pub(super) records: u64,
    pub(super) record_count: u32,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Layout {
    pub(super) counts: Counts,
    pub(super) template_bits: u32,
    pub(super) shape_bits: u32,
    pub(super) ref_bits: u32,
    pub(super) template_offsets: Range<usize>,
    pub(super) templates: Range<usize>,
    pub(super) shape_offsets: Range<usize>,
    pub(super) shapes: Range<usize>,
    pub(super) specs: Range<usize>,
    pub(super) lo: Range<usize>,
    pub(super) sync: Range<usize>,
    pub(super) strings: Range<usize>,
    pub(super) records: Range<usize>,
}

impl Layout {
    pub(super) fn new(counts: Counts) -> Option<Self> {
        let mut at = HEADER_BYTES as u64;
        let mut take = |length: u64| -> Option<Range<usize>> {
            let start = at;
            let end = start.checked_add(length)?;
            at = align8(end)?;
            Some(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
        };
        let sync_count = (counts.record_count as usize).div_ceil(SYNC_EVERY) as u64;
        let layout = Self {
            counts,
            template_bits: bits_for(u64::from(counts.templates)),
            shape_bits: bits_for(u64::from(counts.shapes)),
            ref_bits: bits_for(counts.strings),
            template_offsets: take(4 * (u64::from(counts.templates) + 1))?,
            templates: take(counts.template_bytes.checked_add(SLACK as u64)?)?,
            shape_offsets: take(4 * (u64::from(counts.shapes) + 1))?,
            shapes: take(counts.shape_bytes)?,
            specs: take(SPEC_BYTES as u64 * u64::from(counts.numerics))?,
            lo: take(8 * u64::from(counts.lo_total))?,
            sync: take(4 * sync_count)?,
            strings: take(counts.strings.checked_add(SLACK as u64)?)?,
            records: take(counts.records.checked_add(RECORD_PADDING as u64)?)?,
        };
        Some(layout)
    }

    pub(super) fn length(&self) -> usize {
        self.records.end
    }

    pub(super) fn header(&self) -> [u8; HEADER_BYTES] {
        let counts = &self.counts;
        let mut header = [0u8; HEADER_BYTES];
        header[0..4].copy_from_slice(&counts.templates.to_le_bytes());
        header[4..8].copy_from_slice(&counts.shapes.to_le_bytes());
        header[8..12].copy_from_slice(&counts.numerics.to_le_bytes());
        header[12..16].copy_from_slice(&counts.lo_total.to_le_bytes());
        header[16..24].copy_from_slice(&counts.template_bytes.to_le_bytes());
        header[24..32].copy_from_slice(&counts.shape_bytes.to_le_bytes());
        header[32..40].copy_from_slice(&counts.strings.to_le_bytes());
        header[40..48].copy_from_slice(&counts.records.to_le_bytes());
        header[48..52].copy_from_slice(&counts.record_count.to_le_bytes());
        header[52] = self.template_bits as u8;
        header[53] = self.shape_bits as u8;
        header[54] = self.ref_bits as u8;
        header
    }

    pub(super) fn parse(header: &[u8], length: usize) -> Option<Self> {
        let header: &[u8; HEADER_BYTES] = header.get(..HEADER_BYTES)?.try_into().ok()?;
        let u32_at = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().expect("four"));
        let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().expect("eight"));
        let counts = Counts {
            templates: u32_at(0),
            shapes: u32_at(4),
            numerics: u32_at(8),
            lo_total: u32_at(12),
            template_bytes: u64_at(16),
            shape_bytes: u64_at(24),
            strings: u64_at(32),
            records: u64_at(40),
            record_count: u32_at(48),
        };
        if counts.numerics as usize > MAX_NUMERIC
            || counts.strings >= 1 << 40
            || counts.template_bytes >= 1 << 40
            || counts.shape_bytes >= 1 << 40
            || counts.records >= 1 << 40
            || header[55..64].iter().any(|&byte| byte != 0)
        {
            return None;
        }
        let layout = Self::new(counts)?;
        let widths = [header[52], header[53], header[54]];
        let expected = [layout.template_bits, layout.shape_bits, layout.ref_bits];
        if widths.map(u32::from) != expected || layout.length() != length {
            return None;
        }
        Some(layout)
    }

    pub(super) fn moved(&self, base: usize) -> Self {
        let shift = |range: &Range<usize>| range.start + base..range.end + base;
        Self {
            template_offsets: shift(&self.template_offsets),
            templates: shift(&self.templates),
            shape_offsets: shift(&self.shape_offsets),
            shapes: shift(&self.shapes),
            specs: shift(&self.specs),
            lo: shift(&self.lo),
            sync: shift(&self.sync),
            strings: shift(&self.strings),
            records: shift(&self.records),
            ..self.clone()
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct NumSpec {
    pub(super) shift: u32,
    pub(super) hi_bits: u32,
    pub(super) lo_bits: u32,
    pub(super) lo_start: u32,
    pub(super) lo_count: u32,
}

impl NumSpec {
    pub(super) fn to_bytes(self) -> [u8; SPEC_BYTES] {
        let mut bytes = [0u8; SPEC_BYTES];
        bytes[0] = self.shift as u8;
        bytes[1] = self.hi_bits as u8;
        bytes[2] = self.lo_bits as u8;
        bytes[4..8].copy_from_slice(&self.lo_start.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.lo_count.to_le_bytes());
        bytes
    }

    pub(super) fn parse(bytes: &[u8], lo_total: u32) -> Option<Self> {
        let spec = Self {
            shift: u32::from(bytes[0]),
            hi_bits: u32::from(bytes[1]),
            lo_bits: u32::from(bytes[2]),
            lo_start: u32::from_le_bytes(bytes[4..8].try_into().ok()?),
            lo_count: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
        };
        let valid = bytes[3] == 0
            && bytes[12..SPEC_BYTES] == [0; 4]
            && spec.shift <= 63
            && spec.hi_bits + spec.shift <= 64
            && spec.lo_count >= 1
            && spec.lo_bits == bits_for(u64::from(spec.lo_count))
            && u64::from(spec.lo_start) + u64::from(spec.lo_count) <= u64::from(lo_total);
        valid.then_some(spec)
    }
}

#[inline(always)]
pub(super) fn read_bits(bytes: &[u8], bit: usize, width: u32) -> u64 {
    if width <= 56 {
        read_narrow(bytes, bit, width)
    } else {
        read_narrow(bytes, bit, 32) | read_narrow(bytes, bit + 32, width - 32) << 32
    }
}

#[inline(always)]
fn read_narrow(bytes: &[u8], bit: usize, width: u32) -> u64 {
    let word = u64::from_le_bytes(
        bytes[bit >> 3..(bit >> 3) + 8]
            .try_into()
            .expect("eight bytes"),
    );
    (word >> (bit & 7)) & ((1u64 << width) - 1)
}

#[derive(Default)]
pub(super) struct BitWriter {
    pub(super) bytes: Vec<u8>,
    accumulator: u64,
    count: u32,
}

impl BitWriter {
    pub(super) fn put(&mut self, mut value: u64, mut width: u32) {
        while width > 0 {
            let part = width.min(32);
            let chunk = value & ((1u64 << part) - 1);
            self.accumulator |= chunk << self.count;
            self.count += part;
            while self.count >= 8 {
                self.bytes.push(self.accumulator as u8);
                self.accumulator >>= 8;
                self.count -= 8;
            }
            value = value.checked_shr(part).unwrap_or(0);
            width -= part;
        }
    }

    pub(super) fn put_bytes(&mut self, data: &[u8]) {
        self.finish_record();
        self.bytes.extend_from_slice(data);
        self.bytes.push(0);
    }

    pub(super) fn finish_record(&mut self) {
        if self.count > 0 {
            self.bytes.push(self.accumulator as u8);
            self.accumulator = 0;
            self.count = 0;
        }
    }
}

pub(crate) trait Sink {
    fn put(&mut self, text: &str) -> fmt::Result;

    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result;
}

impl Sink for String {
    #[inline(always)]
    fn put(&mut self, text: &str) -> fmt::Result {
        self.push_str(text);
        Ok(())
    }

    #[inline(always)]
    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result {
        let window = &region[start..start + length + SLACK];
        unsafe {
            let vector = self.as_mut_vec();
            vector.reserve(length + SLACK);
            let end = vector.len();
            let target = vector.as_mut_ptr().add(end);
            let source = window.as_ptr();
            let mut done = 0;
            loop {
                std::ptr::copy_nonoverlapping(source.add(done), target.add(done), 16);
                done += 16;
                if done >= length {
                    break;
                }
            }
            vector.set_len(end + length);
        }
        Ok(())
    }
}

impl Sink for fmt::Formatter<'_> {
    fn put(&mut self, text: &str) -> fmt::Result {
        self.write_str(text)
    }

    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result {
        self.write_str(unsafe { text_of(&region[start..start + length]) })
    }
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize
}

#[inline(always)]
pub(super) fn piece_at(program: &[u8], at: usize) -> (usize, usize) {
    let first = program[at];
    if first < 255 {
        (at + 1, usize::from(first))
    } else {
        (at + 5, u32_at(program, at + 1))
    }
}

pub(super) fn push_piece(program: &mut Vec<u8>, piece: &[u8]) {
    if piece.len() < 255 {
        program.push(piece.len() as u8);
    } else {
        program.push(255);
        program.extend_from_slice(&(piece.len() as u32).to_le_bytes());
    }
    program.extend_from_slice(piece);
}

#[derive(Clone, Copy)]
pub(crate) struct FeatureView<'a> {
    pub(super) template_offsets: &'a [u8],
    pub(super) templates: &'a [u8],
    pub(super) shape_offsets: &'a [u8],
    pub(super) shapes: &'a [u8],
    pub(super) lo: &'a [u8],
    pub(super) strings: &'a [u8],
    pub(super) records: &'a [u8],
    pub(super) specs: &'a [NumSpec],
    pub(super) template_bits: u32,
    pub(super) shape_bits: u32,
    pub(super) ref_bits: u32,
}

#[inline(always)]
unsafe fn text_of(bytes: &[u8]) -> &str {
    unsafe { str::from_utf8_unchecked(bytes) }
}

const IN_RECORD: usize = 1 << 62;

impl FeatureView<'_> {
    #[inline(always)]
    fn string(&self, at: usize) -> (usize, usize) {
        let tail = &self.strings[at..];
        if tail.first().is_some_and(|&byte| byte.cast_signed() < -64) {
            return (at, 0);
        }
        (at, find_byte(tail, 0))
    }

    #[inline]
    pub(crate) fn emit<S: Sink + ?Sized>(&self, offset: u32, sink: &mut S) -> fmt::Result {
        let record = &self.records[offset as usize..];
        let mut bit = 0usize;
        let template = read_bits(record, bit, self.template_bits) as usize;
        bit += self.template_bits as usize;
        let shape = read_bits(record, bit, self.shape_bits) as usize;
        bit += self.shape_bits as usize;
        let begin = u32_at(self.template_offsets, 4 * template);
        let end = u32_at(self.template_offsets, 4 * template + 4);
        let ops = &self.shapes
            [u32_at(self.shape_offsets, 4 * shape)..u32_at(self.shape_offsets, 4 * shape + 4)];
        let mut slots = [(0usize, 0usize); MAX_SLOTS];
        let mut slot_count = 0;
        let mut highs = [0u64; MAX_NUMERIC];
        let mut at = begin;
        let mut next_op = 0;
        loop {
            let (start, length) = piece_at(self.templates, at);
            unsafe { sink.put_region(self.templates, start, length)? };
            at = start + length;
            let Some(&op) = ops.get(next_op) else {
                return Ok(());
            };
            if at >= end {
                return Ok(());
            }
            next_op += 1;
            match op {
                OP_REF => {
                    let target = read_bits(record, bit, self.ref_bits) as usize;
                    bit += self.ref_bits as usize;
                    let (start, length) = self.string(target);
                    if slot_count < MAX_SLOTS {
                        slots[slot_count] = (start, length);
                        slot_count += 1;
                    }
                    unsafe { sink.put_region(self.strings, start, length)? };
                }
                OP_INLINE => {
                    let start = bit.div_ceil(8);
                    let length = find_byte(&record[start..], 0);
                    bit = (start + length + 1) * 8;
                    if slot_count < MAX_SLOTS {
                        slots[slot_count] = (start | IN_RECORD, length);
                        slot_count += 1;
                    }
                    sink.put(str::from_utf8(&record[start..start + length]).unwrap_or(""))?;
                }
                OP_EMPTY => {}
                OP_STAR => sink.put("*")?,
                OP_COPY..=0x1F => {
                    let (start, length) = slots[usize::from(op - OP_COPY)];
                    if start & IN_RECORD != 0 {
                        let start = start & !IN_RECORD;
                        sink.put(str::from_utf8(&record[start..start + length]).unwrap_or(""))?;
                    } else {
                        unsafe { sink.put_region(self.strings, start, length)? };
                    }
                }
                OP_NUM..=0x23 => {
                    let number = usize::from(op - OP_NUM);
                    let spec = &self.specs[number];
                    let high = read_bits(record, bit, spec.hi_bits);
                    bit += spec.hi_bits as usize;
                    let index = read_bits(record, bit, spec.lo_bits) as usize;
                    bit += spec.lo_bits as usize;
                    let low_at = 8 * (spec.lo_start as usize + index);
                    let low = u64::from_le_bytes(
                        self.lo[low_at..low_at + 8].try_into().expect("eight bytes"),
                    );
                    highs[number] = high;
                    put_number(sink, high << spec.shift | low)?;
                }
                OP_HIGH..=0x33 => put_number(sink, highs[usize::from(op - OP_HIGH)])?,
                _ => {}
            }
        }
    }
}

#[inline(always)]
pub(super) fn find_byte(bytes: &[u8], needle: u8) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let pattern = ONES * u64::from(needle);
    let (chunks, rest) = bytes.as_chunks::<8>();
    let mut at = 0;
    for &chunk in chunks {
        let word = u64::from_le_bytes(chunk) ^ pattern;
        let zeros = word.wrapping_sub(ONES) & !word & (ONES << 7);
        if zeros != 0 {
            return at + (zeros.trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    at + rest
        .iter()
        .position(|&byte| byte == needle)
        .unwrap_or(rest.len())
}

const DIGIT_PAIRS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

fn put_number<S: Sink + ?Sized>(sink: &mut S, mut value: u64) -> fmt::Result {
    let mut digits = [0u8; 20 + SLACK];
    let mut at = 20;
    while value >= 10_000 {
        let rest = (value % 10_000) as usize;
        value /= 10_000;
        at -= 4;
        let (high, low) = (rest / 100 * 2, rest % 100 * 2);
        digits[at..at + 2].copy_from_slice(&DIGIT_PAIRS[high..high + 2]);
        digits[at + 2..at + 4].copy_from_slice(&DIGIT_PAIRS[low..low + 2]);
    }
    let mut rest = value as usize;
    if rest >= 100 {
        at -= 2;
        let low = rest % 100 * 2;
        digits[at..at + 2].copy_from_slice(&DIGIT_PAIRS[low..low + 2]);
        rest /= 100;
    }
    if rest >= 10 {
        at -= 2;
        digits[at..at + 2].copy_from_slice(&DIGIT_PAIRS[rest * 2..rest * 2 + 2]);
    } else {
        at -= 1;
        digits[at] = b'0' + rest as u8;
    }
    unsafe { sink.put_region(&digits, at, 20 - at) }
}
