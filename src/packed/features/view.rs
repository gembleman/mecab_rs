use std::fmt;
use std::ops::Range;

use super::number::put_number;
use super::sink::{Cursor, Sink};
use super::{
    MAX_NUMERIC, MAX_SLOTS, NO_RULE, NumSpec, OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_COPY_MAX,
    OP_DERIVE, OP_EMPTY, OP_HIGH, OP_HIGH_MAX, OP_INLINE, OP_KEY, OP_NUM, OP_NUM_MAX, OP_NUMB,
    OP_NUMB_MAX, OP_REF, OP_STAR, RULE_BYTES, RULE_CHAR_BITS, RULE_CHAR_MASK, RULE_HEADER_BYTES,
    access, check, pieces, read_bits, read_small,
};
use crate::packed::bits::narrow_mask;

pub(in crate::packed) fn push_piece(program: &mut Vec<u8>, piece: &[u8]) {
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
    pub(in crate::packed) positions: &'a [u8],
    pub(in crate::packed) piece_offsets: &'a [u8],
    pub(in crate::packed) pool: &'a [u8],
    pub(in crate::packed) entries: &'a [u8],
    pub(in crate::packed) entry_bits: u32,
    pub(in crate::packed) shape_offsets: &'a [u8],
    pub(in crate::packed) shapes: &'a [u8],
    pub(in crate::packed) lo: &'a [u8],
    pub(in crate::packed) chars: &'a [u8],
    pub(in crate::packed) strings: &'a [u8],
    pub(in crate::packed) blob: &'a [u8],
    pub(in crate::packed) template_groups: &'a [u8],
    pub(in crate::packed) group_starts: &'a [u8],
    pub(in crate::packed) rule_entries: &'a [u8],
    pub(in crate::packed) rules: &'a [u8],
    pub(in crate::packed) records: &'a [u8],
    pub(in crate::packed) specs: &'a [NumSpec],
    pub(in crate::packed) template_bits: u32,
    pub(in crate::packed) shape_bits: u32,
    pub(in crate::packed) ref_bits: u32,
    pub(in crate::packed) blob_bits: u32,
    pub(in crate::packed) id_bytes: usize,
}

impl<'a> FeatureView<'a> {
    #[inline(always)]
    pub(in crate::packed) fn new(
        map: &'a [u8],
        mapped: &check::MappedLayout,
        specs: &'a [NumSpec],
    ) -> Self {
        let layout = mapped.layout();
        let part = |range: &Range<usize>| {
            debug_assert!(range.start <= range.end && range.end <= map.len());
            unsafe { map.get_unchecked(range.start..range.end) }
        };
        Self {
            positions: part(&layout.positions),
            piece_offsets: part(&layout.piece_offsets),
            pool: part(&layout.pool),
            entries: part(&layout.entries),
            entry_bits: layout.counts.entry_bits,
            shape_offsets: part(&layout.shape_offsets),
            shapes: part(&layout.shapes),
            lo: part(&layout.lo),
            chars: part(&layout.chars),
            strings: part(&layout.strings),
            blob: part(&layout.blob),
            template_groups: part(&layout.template_groups),
            group_starts: part(&layout.group_starts),
            rule_entries: part(&layout.rule_entries),
            rules: part(&layout.rules),
            records: part(&layout.records),
            specs,
            template_bits: layout.template_bits,
            shape_bits: layout.shape_bits,
            ref_bits: layout.ref_bits,
            blob_bits: layout.blob_bits,
            id_bytes: layout.counts.id_bytes as usize,
        }
    }
}

const RECORD_REGION_BIT: u32 = 62;
const BLOB_REGION_BIT: u32 = 61;
const DERIVED_REGION_BIT: u32 = 60;
const IN_RECORD: usize = 1 << RECORD_REGION_BIT;
const IN_BLOB: usize = 1 << BLOB_REGION_BIT;
const IN_DERIVED: usize = 1 << DERIVED_REGION_BIT;
const REGION_FLAGS: usize = IN_RECORD | IN_BLOB | IN_DERIVED;

#[inline(always)]
const fn region_of(position: usize) -> usize {
    position & REGION_FLAGS
}

#[inline(always)]
const fn offset_of(position: usize) -> usize {
    position & !REGION_FLAGS
}

impl FeatureView<'_> {
    #[inline(always)]
    fn string<const TRUSTED: bool>(&self, at: usize) -> (usize, usize) {
        let tail = access::from::<TRUSTED>(self.strings, at);
        (at, find_byte(tail, 0))
    }

    #[inline]
    pub(crate) unsafe fn emit_into(&self, offset: u32, key: &str, out: &mut String) {
        let mut cursor = Cursor::new(out);
        match self.emit_with::<true, _>(offset, key, &mut cursor) {
            Ok(()) => {}
            Err(_) => unreachable!("writing to a string does not fail"),
        }
        cursor.finish();
    }

    #[inline]
    pub(crate) fn emit<S: Sink + ?Sized>(
        &self,
        offset: u32,
        key: &str,
        sink: &mut S,
    ) -> fmt::Result {
        self.emit_with::<false, S>(offset, key, sink)
    }

    #[inline(always)]
    fn emit_with<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        offset: u32,
        key: &str,
        sink: &mut S,
    ) -> fmt::Result {
        let record = access::from::<TRUSTED>(self.records, offset as usize);
        let mut walk = Walk::new(record, key);
        walk.template = read_small::<TRUSTED>(record, walk.bit, self.template_bits) as usize;
        walk.bit += self.template_bits as usize;
        let shape = read_small::<TRUSTED>(record, walk.bit, self.shape_bits) as usize;
        walk.bit += self.shape_bits as usize;
        let entry_bit = walk.template * self.entry_bits as usize;
        let ops = access::range::<TRUSTED>(
            self.shapes,
            access::u32_at::<TRUSTED>(self.shape_offsets, 4 * shape)
                ..access::u32_at::<TRUSTED>(self.shape_offsets, 4 * shape + 4),
        );
        for (position, &op) in ops.iter().enumerate() {
            unsafe { self.put_piece::<TRUSTED, S>(sink, entry_bit, position)? };
            unsafe { self.emit_op::<TRUSTED, S>(op, &mut walk, sink)? };
        }
        unsafe { self.put_piece::<TRUSTED, S>(sink, entry_bit, ops.len())? };
        Ok(())
    }

    #[inline(always)]
    unsafe fn emit_op<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        op: u8,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        match op {
            OP_REF => unsafe { self.emit_ref::<TRUSTED, S>(walk, sink) },
            OP_INLINE => unsafe { self.emit_inline::<TRUSTED, S>(walk, sink) },
            OP_EMPTY => Ok(()),
            OP_STAR => sink.put("*"),
            OP_KEY => sink.put(walk.key),
            OP_BLOB | OP_BLOB_NEXT => unsafe { self.emit_blob::<TRUSTED, S>(op, walk, sink) },
            OP_DERIVE => unsafe { self.emit_derive::<TRUSTED, S>(walk, sink) },
            OP_COPY..=OP_COPY_MAX => unsafe { self.emit_copy::<TRUSTED, S>(op, walk, sink) },
            OP_NUM..=OP_NUM_MAX => self.emit_num::<TRUSTED, S>(op, walk, sink),
            OP_NUMB..=OP_NUMB_MAX => self.emit_numb::<TRUSTED, S>(op, walk, sink),
            OP_HIGH..=OP_HIGH_MAX => put_number(sink, walk.highs[usize::from(op - OP_HIGH)]),
            _ => Ok(()),
        }
    }

    #[inline(always)]
    unsafe fn put_codes<S: Sink + ?Sized>(&self, sink: &mut S, codes: &[u8]) -> fmt::Result {
        unsafe { sink.put_coded(self.chars, codes) }
    }

    #[inline(always)]
    unsafe fn emit_ref<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        let target = read_small::<TRUSTED>(walk.record, walk.bit, self.ref_bits) as usize;
        walk.bit += self.ref_bits as usize;
        let (start, length) = self.string::<TRUSTED>(target);
        walk.slots.push((start, length));
        let codes = access::range::<TRUSTED>(self.strings, start..start + length);
        unsafe { self.put_codes(sink, codes) }
    }

    #[inline(always)]
    unsafe fn emit_inline<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        let start = walk.bit.div_ceil(8);
        let length = find_byte(access::from::<TRUSTED>(walk.record, start), 0);
        walk.bit = (start + length + 1) * 8;
        walk.slots.push((start | IN_RECORD, length));
        let codes = access::range::<TRUSTED>(walk.record, start..start + length);
        unsafe { self.put_codes(sink, codes) }
    }

    #[inline(always)]
    unsafe fn emit_blob<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        op: u8,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        if op == OP_BLOB {
            walk.blob_at = read_small::<TRUSTED>(walk.record, walk.bit, self.blob_bits) as usize;
            walk.bit += self.blob_bits as usize;
            walk.blob_next = walk.blob_at + self.id_bytes;
        }
        let start = walk.blob_next;
        let length = find_byte(access::from::<TRUSTED>(self.blob, start), 0);
        walk.blob_next = start + length + 1;
        walk.slots.push((start | IN_BLOB, length));
        let codes = access::range::<TRUSTED>(self.blob, start..start + length);
        unsafe { self.put_codes(sink, codes) }
    }

    #[inline(always)]
    unsafe fn emit_derive<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        let (rule, keep) = self.derive::<TRUSTED>(walk.template, walk.key);
        walk.slots.push((rule | IN_DERIVED, keep));
        unsafe { self.put_derived::<TRUSTED, S>(sink, walk.key, rule, keep) }
    }

    #[inline(always)]
    unsafe fn emit_copy<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        op: u8,
        walk: &Walk,
        sink: &mut S,
    ) -> fmt::Result {
        let (start, length) = walk.slots.get(usize::from(op - OP_COPY));
        let region = match region_of(start) {
            0 => self.strings,
            IN_RECORD => walk.record,
            IN_BLOB => self.blob,
            _ => {
                let rule = offset_of(start);
                return unsafe { self.put_derived::<TRUSTED, S>(sink, walk.key, rule, length) };
            }
        };
        let start = offset_of(start);
        let codes = access::range::<TRUSTED>(region, start..start + length);
        unsafe { self.put_codes(sink, codes) }
    }

    #[inline(always)]
    fn emit_num<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        op: u8,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        let number = usize::from(op - OP_NUM);
        let spec = &self.specs[number];
        let high = read_bits(walk.record, walk.bit, spec.hi_bits);
        walk.bit += spec.hi_bits as usize;
        self.put_split_number::<TRUSTED, S>(number, high, walk, sink)
    }

    #[inline(always)]
    fn emit_numb<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        op: u8,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        let number = usize::from(op - OP_NUMB);
        let id_mask = narrow_mask(8 * self.id_bytes as u32);
        let high = access::word::<TRUSTED>(self.blob, walk.blob_at) & id_mask;
        self.put_split_number::<TRUSTED, S>(number, high, walk, sink)
    }

    #[inline(always)]
    fn put_split_number<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        number: usize,
        high: u64,
        walk: &mut Walk,
        sink: &mut S,
    ) -> fmt::Result {
        let spec = &self.specs[number];
        let index = read_small::<TRUSTED>(walk.record, walk.bit, spec.lo_bits) as usize;
        walk.bit += spec.lo_bits as usize;
        let low_at = 8 * (spec.lo_start as usize + index);
        let low = access::word::<TRUSTED>(self.lo, low_at);
        walk.highs[number] = high;
        put_number(sink, high << spec.shift | low)
    }

    #[inline(always)]
    fn derive<const TRUSTED: bool>(&self, template: usize, key: &str) -> (usize, usize) {
        const NONE: (usize, usize) = (NO_RULE, 0);
        if self.template_groups.is_empty() {
            return NONE;
        }
        let Some(last) = key.chars().next_back() else {
            return NONE;
        };
        let group = access::u16_at::<TRUSTED>(self.template_groups, 2 * template);
        if group == 0 {
            return NONE;
        }
        let mut low = access::u32_at::<TRUSTED>(self.group_starts, 4 * (group - 1));
        let mut high = access::u32_at::<TRUSTED>(self.group_starts, 4 * group);
        let wanted = last as u32;
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = access::u32_at::<TRUSTED>(self.rule_entries, 4 * middle) as u32;
            let found = entry & RULE_CHAR_MASK;
            if found == wanted {
                let rule = (entry >> RULE_CHAR_BITS) as usize;
                let removed =
                    access::range::<TRUSTED>(self.rules, rule * RULE_BYTES..rule * RULE_BYTES + 1)
                        [0];
                let mut keep = key.len();
                for _ in 0..removed {
                    match key[..keep].chars().next_back() {
                        Some(ch) => keep -= ch.len_utf8(),
                        None => break,
                    }
                }
                return (rule, keep);
            }
            if found < wanted {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        NONE
    }

    #[inline(always)]
    unsafe fn put_derived<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        sink: &mut S,
        key: &str,
        rule: usize,
        keep: usize,
    ) -> fmt::Result {
        if rule == NO_RULE {
            return Ok(());
        }
        sink.put(&key[..keep])?;
        let at = rule * RULE_BYTES;
        let length = usize::from(access::range::<TRUSTED>(self.rules, at + 1..at + 2)[0]);
        unsafe { sink.put_region(self.rules, at + RULE_HEADER_BYTES, length) }
    }

    #[inline(always)]
    unsafe fn put_piece<const TRUSTED: bool, S: Sink + ?Sized>(
        &self,
        sink: &mut S,
        entry_bit: usize,
        position: usize,
    ) -> fmt::Result {
        let word = access::word::<TRUSTED>(self.positions, pieces::POSITION_BYTES * position);
        let (base, shift, bits) = pieces::unpack_position(word);
        if word & pieces::CONSTANT_FLAG != 0 {
            return unsafe { sink.put_region(self.pool, base, shift) };
        }
        let local = read_small::<TRUSTED>(self.entries, entry_bit + shift, bits) as usize;
        let number = base + local;
        let start = access::u32_at::<TRUSTED>(self.piece_offsets, 4 * number);
        let end = access::u32_at::<TRUSTED>(self.piece_offsets, 4 * number + 4);
        unsafe { sink.put_region(self.pool, start, end - start) }
    }
}

#[inline(always)]
pub(in crate::packed) fn find_byte(bytes: &[u8], needle: u8) -> usize {
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

struct Slots {
    items: [(usize, usize); MAX_SLOTS],
    count: usize,
}

impl Slots {
    #[inline(always)]
    fn new() -> Self {
        Self {
            items: [(0, 0); MAX_SLOTS],
            count: 0,
        }
    }

    #[inline(always)]
    fn push(&mut self, slot: (usize, usize)) {
        if self.count < MAX_SLOTS {
            self.items[self.count] = slot;
            self.count += 1;
        }
    }

    #[inline(always)]
    fn get(&self, index: usize) -> (usize, usize) {
        self.items[index]
    }
}

struct Walk<'r> {
    record: &'r [u8],
    key: &'r str,
    template: usize,
    bit: usize,
    slots: Slots,
    highs: [u64; MAX_NUMERIC],
    blob_at: usize,
    blob_next: usize,
}

impl<'r> Walk<'r> {
    #[inline(always)]
    fn new(record: &'r [u8], key: &'r str) -> Self {
        Self {
            record,
            key,
            template: 0,
            bit: 0,
            slots: Slots::new(),
            highs: [0; MAX_NUMERIC],
            blob_at: 0,
            blob_next: 0,
        }
    }
}
