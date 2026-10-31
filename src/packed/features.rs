pub(super) mod chars;
pub(super) mod check;
pub(super) mod encode;
pub(super) mod pieces;

use std::fmt;
use std::ops::Range;

use crate::wire::{u32_at, u64_at};

pub(super) const OP_REF: u8 = 0x00;
pub(super) const OP_EMPTY: u8 = 0x01;
pub(super) const OP_STAR: u8 = 0x02;
pub(super) const OP_INLINE: u8 = 0x03;
pub(super) const OP_KEY: u8 = 0x04;
pub(super) const OP_BLOB: u8 = 0x05;
pub(super) const OP_BLOB_NEXT: u8 = 0x06;
pub(super) const OP_DERIVE: u8 = 0x07;
pub(super) const OP_COPY: u8 = 0x10;
pub(super) const OP_NUM: u8 = 0x20;
pub(super) const OP_HIGH: u8 = 0x30;
pub(super) const OP_NUMB: u8 = 0x40;
pub(super) const MAX_SLOTS: usize = 16;
pub(super) const MAX_NUMERIC: usize = 4;
pub(super) const SPEC_BYTES: usize = 16;
pub(super) const HEADER_BYTES: usize = 104;
pub(super) const MAX_BLOB_FIELDS: usize = 4;
pub(super) const MAX_ID_BYTES: usize = 4;
pub(super) const SYNC_EVERY: usize = 4096;
pub(super) const BLOB_SYNC_EVERY: usize = 1024;
pub(super) const RULE_BYTES: usize = 16;
pub(super) const RULE_HEADER_BYTES: usize = 2;
pub(super) const MAX_RULE_SUFFIX: usize = RULE_BYTES - RULE_HEADER_BYTES;
pub(super) const RULE_CHAR_BITS: u32 = 21;
pub(super) const RULE_CHAR_MASK: u32 = (1 << RULE_CHAR_BITS) - 1;
pub(super) const MAX_RULES: usize = 1 << (32 - RULE_CHAR_BITS);
pub(super) const NO_RULE: usize = usize::MAX >> 4;
pub(super) const RECORD_PADDING: usize = 16;
pub(super) const SLACK: usize = 16;
pub(super) const OP_COPY_MAX: u8 = OP_NUM - 1;
pub(super) const OP_NUM_MAX: u8 = OP_NUM + MAX_NUMERIC as u8 - 1;
pub(super) const OP_HIGH_MAX: u8 = OP_HIGH + MAX_NUMERIC as u8 - 1;
pub(super) const OP_NUMB_MAX: u8 = OP_NUMB + MAX_NUMERIC as u8 - 1;
const TEMPLATES_AT: usize = 0;
const SHAPES_AT: usize = 4;
const NUMERICS_AT: usize = 8;
const LO_TOTAL_AT: usize = 12;
const POOL_BYTES_AT: usize = 16;
const SHAPE_BYTES_AT: usize = 24;
const STRINGS_AT: usize = 32;
const RECORDS_AT: usize = 40;
const RECORD_COUNT_AT: usize = 48;
const CHARS_AT: usize = 56;
const PIECES_AT: usize = 60;
const POSITIONS_AT: usize = 64;
const ENTRY_BITS_AT: usize = 68;
const BLOB_BYTES_AT: usize = 72;
const BLOB_ENTRIES_AT: usize = 84;
const RULE_GROUPS_AT: usize = 88;
const RULE_ENTRIES_AT: usize = 92;
const RULE_COUNT_AT: usize = 96;
const TEMPLATE_BITS_AT: usize = 52;
const SHAPE_BITS_AT: usize = 53;
const REF_BITS_AT: usize = 54;
const HEADER_ZERO_AT: usize = 55;
const BLOB_FIELDS_AT: usize = 80;
const BLOB_NUMBER_AT: usize = 81;
const ID_BYTES_AT: usize = 82;
const BLOB_BITS_AT: usize = 83;
const HEADER_TAIL_AT: usize = 100;
const LENGTH_LIMIT: u64 = 1 << 40;
const MAX_DIGITS: usize = 20;

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
    pub(super) chars: u32,
    pub(super) pieces: u32,
    pub(super) positions: u32,
    pub(super) entry_bits: u32,
    pub(super) pool_bytes: u64,
    pub(super) shape_bytes: u64,
    pub(super) strings: u64,
    pub(super) blob_bytes: u64,
    pub(super) blob_entries: u32,
    pub(super) rule_groups: u32,
    pub(super) rule_entries: u32,
    pub(super) rule_count: u32,
    pub(super) blob_fields: u32,
    pub(super) blob_number: u32,
    pub(super) id_bytes: u32,
    pub(super) records: u64,
    pub(super) record_count: u32,
}

impl Counts {
    fn blob_is_consistent(&self) -> bool {
        if self.blob_fields == 0 {
            return self.blob_bytes == 0
                && self.blob_entries == 0
                && self.blob_number == 0
                && self.id_bytes == 0;
        }
        self.blob_fields as usize <= MAX_BLOB_FIELDS
            && (self.blob_number as usize) < MAX_NUMERIC
            && (1..=MAX_ID_BYTES as u32).contains(&self.id_bytes)
            && self.blob_entries >= 1
            && u64::from(self.blob_entries)
                * (u64::from(self.id_bytes) + u64::from(self.blob_fields))
                <= self.blob_bytes
    }
}

impl Counts {
    fn rules_are_consistent(&self) -> bool {
        if self.rule_groups == 0 {
            return self.rule_entries == 0 && self.rule_count == 0;
        }
        self.rule_count as usize <= MAX_RULES
            && self.rule_groups <= self.rule_entries
            && self.rule_count >= 1
            && u16::try_from(self.rule_groups).is_ok()
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Layout {
    pub(super) counts: Counts,
    pub(super) template_bits: u32,
    pub(super) shape_bits: u32,
    pub(super) ref_bits: u32,
    pub(super) blob_bits: u32,
    pub(super) positions: Range<usize>,
    pub(super) piece_offsets: Range<usize>,
    pub(super) pool: Range<usize>,
    pub(super) entries: Range<usize>,
    pub(super) shape_offsets: Range<usize>,
    pub(super) shapes: Range<usize>,
    pub(super) specs: Range<usize>,
    pub(super) lo: Range<usize>,
    pub(super) chars: Range<usize>,
    pub(super) sync: Range<usize>,
    pub(super) strings: Range<usize>,
    pub(super) blob_sync: Range<usize>,
    pub(super) blob: Range<usize>,
    pub(super) template_groups: Range<usize>,
    pub(super) group_starts: Range<usize>,
    pub(super) rule_entries: Range<usize>,
    pub(super) rules: Range<usize>,
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
        let blob_sync_count = (counts.blob_entries as usize).div_ceil(BLOB_SYNC_EVERY) as u64;
        let layout = Self {
            counts,
            template_bits: bits_for(u64::from(counts.templates)),
            shape_bits: bits_for(u64::from(counts.shapes)),
            ref_bits: bits_for(counts.strings),
            blob_bits: bits_for(counts.blob_bytes),
            positions: take(pieces::POSITION_BYTES as u64 * u64::from(counts.positions))?,
            piece_offsets: take(4 * (u64::from(counts.pieces) + 1))?,
            pool: take(counts.pool_bytes.checked_add(SLACK as u64)?)?,
            entries: take(
                (u64::from(counts.templates) * u64::from(counts.entry_bits))
                    .div_ceil(8)
                    .checked_add(SLACK as u64)?,
            )?,
            shape_offsets: take(4 * (u64::from(counts.shapes) + 1))?,
            shapes: take(counts.shape_bytes)?,
            specs: take(SPEC_BYTES as u64 * u64::from(counts.numerics))?,
            lo: take(8 * u64::from(counts.lo_total))?,
            chars: take(chars::ENTRY_BYTES as u64 * u64::from(counts.chars))?,
            sync: take(4 * sync_count)?,
            strings: take(counts.strings.checked_add(SLACK as u64)?)?,
            blob_sync: take(4 * blob_sync_count)?,
            blob: take(counts.blob_bytes.checked_add(SLACK as u64)?)?,
            template_groups: take(if counts.rule_groups == 0 {
                0
            } else {
                2 * u64::from(counts.templates)
            })?,
            group_starts: take(if counts.rule_groups == 0 {
                0
            } else {
                4 * (u64::from(counts.rule_groups) + 1)
            })?,
            rule_entries: take(4 * u64::from(counts.rule_entries))?,
            rules: take(RULE_BYTES as u64 * u64::from(counts.rule_count) + SLACK as u64)?,
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
        header[TEMPLATES_AT..][..4].copy_from_slice(&counts.templates.to_le_bytes());
        header[SHAPES_AT..][..4].copy_from_slice(&counts.shapes.to_le_bytes());
        header[NUMERICS_AT..][..4].copy_from_slice(&counts.numerics.to_le_bytes());
        header[LO_TOTAL_AT..][..4].copy_from_slice(&counts.lo_total.to_le_bytes());
        header[POOL_BYTES_AT..][..8].copy_from_slice(&counts.pool_bytes.to_le_bytes());
        header[SHAPE_BYTES_AT..][..8].copy_from_slice(&counts.shape_bytes.to_le_bytes());
        header[STRINGS_AT..][..8].copy_from_slice(&counts.strings.to_le_bytes());
        header[RECORDS_AT..][..8].copy_from_slice(&counts.records.to_le_bytes());
        header[RECORD_COUNT_AT..][..4].copy_from_slice(&counts.record_count.to_le_bytes());
        header[TEMPLATE_BITS_AT] = self.template_bits as u8;
        header[SHAPE_BITS_AT] = self.shape_bits as u8;
        header[REF_BITS_AT] = self.ref_bits as u8;
        header[CHARS_AT..][..4].copy_from_slice(&counts.chars.to_le_bytes());
        header[PIECES_AT..][..4].copy_from_slice(&counts.pieces.to_le_bytes());
        header[POSITIONS_AT..][..4].copy_from_slice(&counts.positions.to_le_bytes());
        header[ENTRY_BITS_AT..][..4].copy_from_slice(&counts.entry_bits.to_le_bytes());
        header[BLOB_BYTES_AT..][..8].copy_from_slice(&counts.blob_bytes.to_le_bytes());
        header[BLOB_FIELDS_AT] = counts.blob_fields as u8;
        header[BLOB_NUMBER_AT] = counts.blob_number as u8;
        header[ID_BYTES_AT] = counts.id_bytes as u8;
        header[BLOB_BITS_AT] = self.blob_bits as u8;
        header[BLOB_ENTRIES_AT..][..4].copy_from_slice(&counts.blob_entries.to_le_bytes());
        header[RULE_GROUPS_AT..][..4].copy_from_slice(&counts.rule_groups.to_le_bytes());
        header[RULE_ENTRIES_AT..][..4].copy_from_slice(&counts.rule_entries.to_le_bytes());
        header[RULE_COUNT_AT..][..4].copy_from_slice(&counts.rule_count.to_le_bytes());
        header
    }

    pub(super) fn parse(header: &[u8], length: usize) -> Option<Self> {
        let header: &[u8; HEADER_BYTES] = header.get(..HEADER_BYTES)?.try_into().ok()?;
        let counts = Counts {
            templates: u32_at(header, TEMPLATES_AT),
            shapes: u32_at(header, SHAPES_AT),
            numerics: u32_at(header, NUMERICS_AT),
            lo_total: u32_at(header, LO_TOTAL_AT),
            chars: u32_at(header, CHARS_AT),
            pieces: u32_at(header, PIECES_AT),
            positions: u32_at(header, POSITIONS_AT),
            entry_bits: u32_at(header, ENTRY_BITS_AT),
            pool_bytes: u64_at(header, POOL_BYTES_AT),
            shape_bytes: u64_at(header, SHAPE_BYTES_AT),
            strings: u64_at(header, STRINGS_AT),
            records: u64_at(header, RECORDS_AT),
            record_count: u32_at(header, RECORD_COUNT_AT),
            blob_bytes: u64_at(header, BLOB_BYTES_AT),
            blob_entries: u32_at(header, BLOB_ENTRIES_AT),
            rule_groups: u32_at(header, RULE_GROUPS_AT),
            rule_entries: u32_at(header, RULE_ENTRIES_AT),
            rule_count: u32_at(header, RULE_COUNT_AT),
            blob_fields: u32::from(header[BLOB_FIELDS_AT]),
            blob_number: u32::from(header[BLOB_NUMBER_AT]),
            id_bytes: u32::from(header[ID_BYTES_AT]),
        };
        if counts.numerics as usize > MAX_NUMERIC
            || counts.strings >= LENGTH_LIMIT
            || counts.pool_bytes >= LENGTH_LIMIT
            || counts.entry_bits as usize > pieces::MAX_ENTRY_BITS
            || counts.shape_bytes >= LENGTH_LIMIT
            || counts.records >= LENGTH_LIMIT
            || counts.chars as usize > chars::MAX_CHARS
            || counts.blob_bytes >= LENGTH_LIMIT
            || header[HEADER_ZERO_AT] != 0
            || header[HEADER_TAIL_AT..HEADER_BYTES] != [0; HEADER_BYTES - HEADER_TAIL_AT]
            || !counts.blob_is_consistent()
            || !counts.rules_are_consistent()
        {
            return None;
        }
        let layout = Self::new(counts)?;
        let widths = [
            header[TEMPLATE_BITS_AT],
            header[SHAPE_BITS_AT],
            header[REF_BITS_AT],
            header[BLOB_BITS_AT],
        ];
        let expected = [
            layout.template_bits,
            layout.shape_bits,
            layout.ref_bits,
            layout.blob_bits,
        ];
        if widths.map(u32::from) != expected || layout.length() != length {
            return None;
        }
        Some(layout)
    }

    pub(super) fn moved(&self, base: usize) -> Self {
        let shift = |range: &Range<usize>| range.start + base..range.end + base;
        Self {
            positions: shift(&self.positions),
            piece_offsets: shift(&self.piece_offsets),
            pool: shift(&self.pool),
            entries: shift(&self.entries),
            shape_offsets: shift(&self.shape_offsets),
            shapes: shift(&self.shapes),
            specs: shift(&self.specs),
            lo: shift(&self.lo),
            chars: shift(&self.chars),
            sync: shift(&self.sync),
            strings: shift(&self.strings),
            blob_sync: shift(&self.blob_sync),
            blob: shift(&self.blob),
            template_groups: shift(&self.template_groups),
            group_starts: shift(&self.group_starts),
            rule_entries: shift(&self.rule_entries),
            rules: shift(&self.rules),
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
fn read_small<const TRUSTED: bool>(bytes: &[u8], bit: usize, width: u32) -> u64 {
    debug_assert!(width <= 56);
    let word = access::word::<TRUSTED>(bytes, bit >> 3);
    (word >> (bit & 7)) & ((1u64 << width) - 1)
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

    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result;
}

impl Sink for String {
    #[inline(always)]
    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result {
        chars::for_each_char(table, codes, |text| self.put(text))
    }

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

struct Cursor<'a> {
    vector: &'a mut Vec<u8>,
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

#[cold]
#[inline(never)]
fn grow(vector: &mut Vec<u8>, len: usize, extra: usize) -> (*mut u8, usize) {
    unsafe { vector.set_len(len) };
    vector.reserve(extra);
    (vector.as_mut_ptr(), vector.capacity())
}

impl<'a> Cursor<'a> {
    #[inline(always)]
    fn new(string: &'a mut String) -> Self {
        let vector = unsafe { string.as_mut_vec() };
        Self {
            ptr: vector.as_mut_ptr(),
            len: vector.len(),
            cap: vector.capacity(),
            vector,
        }
    }

    #[inline(always)]
    fn reserve(&mut self, extra: usize) {
        if self.cap - self.len < extra {
            (self.ptr, self.cap) = grow(self.vector, self.len, extra);
        }
    }

    #[inline(always)]
    fn finish(self) {
        unsafe { self.vector.set_len(self.len) };
    }
}

impl Sink for Cursor<'_> {
    #[inline(always)]
    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result {
        self.reserve(codes.len() * chars::MAX_EXPANSION + chars::ENTRY_BYTES);
        self.len += unsafe { chars::decode_unchecked(table, codes, self.ptr.add(self.len)) };
        Ok(())
    }

    #[inline(always)]
    fn put(&mut self, text: &str) -> fmt::Result {
        self.reserve(text.len());
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), self.ptr.add(self.len), text.len());
        }
        self.len += text.len();
        Ok(())
    }

    #[inline(always)]
    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result {
        debug_assert!(start + length + SLACK <= region.len());
        if length == 1 {
            self.reserve(1);
            unsafe { *self.ptr.add(self.len) = *region.get_unchecked(start) };
            self.len += 1;
            return Ok(());
        }
        let window = unsafe { region.get_unchecked(start..start + length + SLACK) };
        self.reserve(length + SLACK);
        unsafe {
            let target = self.ptr.add(self.len);
            let source = window.as_ptr();
            let mut done = 0;
            loop {
                std::ptr::copy_nonoverlapping(source.add(done), target.add(done), 16);
                done += 16;
                if done >= length {
                    break;
                }
            }
        }
        self.len += length;
        Ok(())
    }
}

impl Sink for fmt::Formatter<'_> {
    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result {
        chars::for_each_char(table, codes, |text| self.write_str(text))
    }

    fn put(&mut self, text: &str) -> fmt::Result {
        self.write_str(text)
    }

    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result {
        self.write_str(unsafe { text_of(&region[start..start + length]) })
    }
}

mod access {
    use std::ops::Range;

    #[inline(always)]
    pub(super) fn range<const TRUSTED: bool>(bytes: &[u8], range: Range<usize>) -> &[u8] {
        if TRUSTED {
            unsafe { bytes.get_unchecked(range) }
        } else {
            &bytes[range]
        }
    }

    #[inline(always)]
    pub(super) fn from<const TRUSTED: bool>(bytes: &[u8], start: usize) -> &[u8] {
        if TRUSTED {
            unsafe { bytes.get_unchecked(start..) }
        } else {
            &bytes[start..]
        }
    }

    #[inline(always)]
    pub(super) fn word<const TRUSTED: bool>(bytes: &[u8], at: usize) -> u64 {
        if TRUSTED {
            u64::from_le(unsafe { bytes.as_ptr().add(at).cast::<u64>().read_unaligned() })
        } else {
            u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
        }
    }

    #[inline(always)]
    pub(super) fn u16_at<const TRUSTED: bool>(bytes: &[u8], at: usize) -> usize {
        if TRUSTED {
            u16::from_le(unsafe { bytes.as_ptr().add(at).cast::<u16>().read_unaligned() }) as usize
        } else {
            u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes")) as usize
        }
    }

    #[inline(always)]
    pub(super) fn u32_at<const TRUSTED: bool>(bytes: &[u8], at: usize) -> usize {
        if TRUSTED {
            u32::from_le(unsafe { bytes.as_ptr().add(at).cast::<u32>().read_unaligned() }) as usize
        } else {
            u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize
        }
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
    pub(super) positions: &'a [u8],
    pub(super) piece_offsets: &'a [u8],
    pub(super) pool: &'a [u8],
    pub(super) entries: &'a [u8],
    pub(super) entry_bits: u32,
    pub(super) shape_offsets: &'a [u8],
    pub(super) shapes: &'a [u8],
    pub(super) lo: &'a [u8],
    pub(super) chars: &'a [u8],
    pub(super) strings: &'a [u8],
    pub(super) blob: &'a [u8],
    pub(super) template_groups: &'a [u8],
    pub(super) group_starts: &'a [u8],
    pub(super) rule_entries: &'a [u8],
    pub(super) rules: &'a [u8],
    pub(super) records: &'a [u8],
    pub(super) specs: &'a [NumSpec],
    pub(super) template_bits: u32,
    pub(super) shape_bits: u32,
    pub(super) ref_bits: u32,
    pub(super) blob_bits: u32,
    pub(super) id_bytes: usize,
}

#[inline(always)]
unsafe fn text_of(bytes: &[u8]) -> &str {
    unsafe { str::from_utf8_unchecked(bytes) }
}

const IN_RECORD: usize = 1 << 62;
const IN_BLOB: usize = 1 << 61;
const IN_DERIVED: usize = 1 << 60;
const REGION_FLAGS: usize = IN_RECORD | IN_BLOB | IN_DERIVED;

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
        let mut bit = 0usize;
        let template = read_small::<TRUSTED>(record, bit, self.template_bits) as usize;
        bit += self.template_bits as usize;
        let shape = read_small::<TRUSTED>(record, bit, self.shape_bits) as usize;
        bit += self.shape_bits as usize;
        let entry_bit = template * self.entry_bits as usize;
        let ops = access::range::<TRUSTED>(
            self.shapes,
            access::u32_at::<TRUSTED>(self.shape_offsets, 4 * shape)
                ..access::u32_at::<TRUSTED>(self.shape_offsets, 4 * shape + 4),
        );
        let mut slots = [(0usize, 0usize); MAX_SLOTS];
        let mut slot_count = 0;
        let mut highs = [0u64; MAX_NUMERIC];
        let mut blob_at = 0usize;
        let mut blob_next = 0usize;
        for (position, &op) in ops.iter().enumerate() {
            unsafe { self.put_piece::<TRUSTED, S>(sink, entry_bit, position)? };
            match op {
                OP_REF => {
                    let target = read_small::<TRUSTED>(record, bit, self.ref_bits) as usize;
                    bit += self.ref_bits as usize;
                    let (start, length) = self.string::<TRUSTED>(target);
                    if slot_count < MAX_SLOTS {
                        slots[slot_count] = (start, length);
                        slot_count += 1;
                    }
                    let codes = access::range::<TRUSTED>(self.strings, start..start + length);
                    unsafe { sink.put_coded(self.chars, codes)? };
                }
                OP_INLINE => {
                    let start = bit.div_ceil(8);
                    let length = find_byte(access::from::<TRUSTED>(record, start), 0);
                    bit = (start + length + 1) * 8;
                    if slot_count < MAX_SLOTS {
                        slots[slot_count] = (start | IN_RECORD, length);
                        slot_count += 1;
                    }
                    let codes = access::range::<TRUSTED>(record, start..start + length);
                    unsafe { sink.put_coded(self.chars, codes)? };
                }
                OP_EMPTY => {}
                OP_STAR => sink.put("*")?,
                OP_KEY => sink.put(key)?,
                OP_BLOB | OP_BLOB_NEXT => {
                    if op == OP_BLOB {
                        blob_at = read_small::<TRUSTED>(record, bit, self.blob_bits) as usize;
                        bit += self.blob_bits as usize;
                        blob_next = blob_at + self.id_bytes;
                    }
                    let start = blob_next;
                    let tail = access::from::<TRUSTED>(self.blob, start);
                    let length = find_byte(tail, 0);
                    blob_next = start + length + 1;
                    if slot_count < MAX_SLOTS {
                        slots[slot_count] = (start | IN_BLOB, length);
                        slot_count += 1;
                    }
                    let codes = access::range::<TRUSTED>(self.blob, start..start + length);
                    unsafe { sink.put_coded(self.chars, codes)? };
                }
                OP_DERIVE => {
                    let (rule, keep) = self.derive::<TRUSTED>(template, key);
                    if slot_count < MAX_SLOTS {
                        slots[slot_count] = (rule | IN_DERIVED, keep);
                        slot_count += 1;
                    }
                    unsafe { self.put_derived::<TRUSTED, S>(sink, key, rule, keep)? };
                }
                OP_COPY..=OP_COPY_MAX => {
                    let (start, length) = slots[usize::from(op - OP_COPY)];
                    let region = match start & REGION_FLAGS {
                        0 => self.strings,
                        IN_RECORD => record,
                        IN_BLOB => self.blob,
                        _ => {
                            let rule = start & !REGION_FLAGS;
                            unsafe { self.put_derived::<TRUSTED, S>(sink, key, rule, length)? };
                            continue;
                        }
                    };
                    let start = start & !REGION_FLAGS;
                    let codes = access::range::<TRUSTED>(region, start..start + length);
                    unsafe { sink.put_coded(self.chars, codes)? };
                }
                OP_NUM..=OP_NUM_MAX => {
                    let number = usize::from(op - OP_NUM);
                    let spec = &self.specs[number];
                    let high = read_bits(record, bit, spec.hi_bits);
                    bit += spec.hi_bits as usize;
                    let index = read_small::<TRUSTED>(record, bit, spec.lo_bits) as usize;
                    bit += spec.lo_bits as usize;
                    let low_at = 8 * (spec.lo_start as usize + index);
                    let low = access::word::<TRUSTED>(self.lo, low_at);
                    highs[number] = high;
                    put_number(sink, high << spec.shift | low)?;
                }
                OP_NUMB..=OP_NUMB_MAX => {
                    let number = usize::from(op - OP_NUMB);
                    let spec = &self.specs[number];
                    let id_mask = (1u64 << (8 * self.id_bytes)) - 1;
                    let high = access::word::<TRUSTED>(self.blob, blob_at) & id_mask;
                    let index = read_small::<TRUSTED>(record, bit, spec.lo_bits) as usize;
                    bit += spec.lo_bits as usize;
                    let low_at = 8 * (spec.lo_start as usize + index);
                    let low = access::word::<TRUSTED>(self.lo, low_at);
                    highs[number] = high;
                    put_number(sink, high << spec.shift | low)?;
                }
                OP_HIGH..=OP_HIGH_MAX => put_number(sink, highs[usize::from(op - OP_HIGH)])?,
                _ => {}
            }
        }
        unsafe { self.put_piece::<TRUSTED, S>(sink, entry_bit, ops.len())? };
        Ok(())
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
    let mut digits = [0u8; MAX_DIGITS + SLACK];
    let mut at = MAX_DIGITS;
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
    unsafe { sink.put_region(&digits, at, MAX_DIGITS - at) }
}
