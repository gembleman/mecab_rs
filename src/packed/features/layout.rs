use std::ops::Range;

use super::chars;
use super::pieces;
use super::{
    BLOB_SYNC_EVERY, HEADER_BYTES, MAX_BLOB_FIELDS, MAX_ID_BYTES, MAX_NUMERIC, MAX_RULES,
    RECORD_PADDING, RULE_BYTES, SLACK, SPEC_BYTES, SYNC_EVERY,
};
use crate::wire::{u32_at, u64_at};

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

pub(in crate::packed) fn bits_for(count: u64) -> u32 {
    if count <= 1 {
        0
    } else {
        64 - (count - 1).leading_zeros()
    }
}

fn align8(value: u64) -> Option<u64> {
    value.checked_add(7).map(|sum| sum & !7)
}

fn place(start: u64, length: u64) -> Option<(Range<usize>, u64)> {
    let end = start.checked_add(length)?;
    let range = usize::try_from(start).ok()?..usize::try_from(end).ok()?;
    Some((range, align8(end)?))
}

struct Sections {
    next: Option<u64>,
}

impl Sections {
    fn starting_after_header() -> Self {
        Self {
            next: Some(HEADER_BYTES as u64),
        }
    }

    fn take(&mut self, length: Option<u64>) -> Range<usize> {
        let placed = self
            .next
            .zip(length)
            .and_then(|(start, length)| place(start, length));
        self.next = placed.as_ref().map(|(_, next)| *next);
        placed.map_or(0..0, |(range, _)| range)
    }

    fn is_intact(&self) -> bool {
        self.next.is_some()
    }
}

fn template_group_bytes(counts: &Counts) -> u64 {
    if counts.rule_groups == 0 {
        0
    } else {
        2 * u64::from(counts.templates)
    }
}

fn group_start_bytes(counts: &Counts) -> u64 {
    if counts.rule_groups == 0 {
        0
    } else {
        4 * (u64::from(counts.rule_groups) + 1)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(in crate::packed) struct Counts {
    pub(in crate::packed) templates: u32,
    pub(in crate::packed) shapes: u32,
    pub(in crate::packed) numerics: u32,
    pub(in crate::packed) lo_total: u32,
    pub(in crate::packed) chars: u32,
    pub(in crate::packed) pieces: u32,
    pub(in crate::packed) positions: u32,
    pub(in crate::packed) entry_bits: u32,
    pub(in crate::packed) pool_bytes: u64,
    pub(in crate::packed) shape_bytes: u64,
    pub(in crate::packed) strings: u64,
    pub(in crate::packed) blob_bytes: u64,
    pub(in crate::packed) blob_entries: u32,
    pub(in crate::packed) rule_groups: u32,
    pub(in crate::packed) rule_entries: u32,
    pub(in crate::packed) rule_count: u32,
    pub(in crate::packed) blob_fields: u32,
    pub(in crate::packed) blob_number: u32,
    pub(in crate::packed) id_bytes: u32,
    pub(in crate::packed) records: u64,
    pub(in crate::packed) record_count: u32,
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
pub(in crate::packed) struct Layout {
    pub(in crate::packed) counts: Counts,
    pub(in crate::packed) template_bits: u32,
    pub(in crate::packed) shape_bits: u32,
    pub(in crate::packed) ref_bits: u32,
    pub(in crate::packed) blob_bits: u32,
    pub(in crate::packed) positions: Range<usize>,
    pub(in crate::packed) piece_offsets: Range<usize>,
    pub(in crate::packed) pool: Range<usize>,
    pub(in crate::packed) entries: Range<usize>,
    pub(in crate::packed) shape_offsets: Range<usize>,
    pub(in crate::packed) shapes: Range<usize>,
    pub(in crate::packed) specs: Range<usize>,
    pub(in crate::packed) lo: Range<usize>,
    pub(in crate::packed) chars: Range<usize>,
    pub(in crate::packed) sync: Range<usize>,
    pub(in crate::packed) strings: Range<usize>,
    pub(in crate::packed) blob_sync: Range<usize>,
    pub(in crate::packed) blob: Range<usize>,
    pub(in crate::packed) template_groups: Range<usize>,
    pub(in crate::packed) group_starts: Range<usize>,
    pub(in crate::packed) rule_entries: Range<usize>,
    pub(in crate::packed) rules: Range<usize>,
    pub(in crate::packed) records: Range<usize>,
}

impl Layout {
    pub(in crate::packed) fn new(counts: Counts) -> Option<Self> {
        let mut sections = Sections::starting_after_header();
        let sync_count = (counts.record_count as usize).div_ceil(SYNC_EVERY) as u64;
        let blob_sync_count = (counts.blob_entries as usize).div_ceil(BLOB_SYNC_EVERY) as u64;
        let layout = Self {
            counts,
            template_bits: bits_for(u64::from(counts.templates)),
            shape_bits: bits_for(u64::from(counts.shapes)),
            ref_bits: bits_for(counts.strings),
            blob_bits: bits_for(counts.blob_bytes),
            positions: sections.take(Some(
                pieces::POSITION_BYTES as u64 * u64::from(counts.positions),
            )),
            piece_offsets: sections.take(Some(4 * (u64::from(counts.pieces) + 1))),
            pool: sections.take(counts.pool_bytes.checked_add(SLACK as u64)),
            entries: sections.take(
                (u64::from(counts.templates) * u64::from(counts.entry_bits))
                    .div_ceil(8)
                    .checked_add(SLACK as u64),
            ),
            shape_offsets: sections.take(Some(4 * (u64::from(counts.shapes) + 1))),
            shapes: sections.take(Some(counts.shape_bytes)),
            specs: sections.take(Some(SPEC_BYTES as u64 * u64::from(counts.numerics))),
            lo: sections.take(Some(8 * u64::from(counts.lo_total))),
            chars: sections.take(Some(chars::ENTRY_BYTES as u64 * u64::from(counts.chars))),
            sync: sections.take(Some(4 * sync_count)),
            strings: sections.take(counts.strings.checked_add(SLACK as u64)),
            blob_sync: sections.take(Some(4 * blob_sync_count)),
            blob: sections.take(counts.blob_bytes.checked_add(SLACK as u64)),
            template_groups: sections.take(Some(template_group_bytes(&counts))),
            group_starts: sections.take(Some(group_start_bytes(&counts))),
            rule_entries: sections.take(Some(4 * u64::from(counts.rule_entries))),
            rules: sections.take(Some(
                RULE_BYTES as u64 * u64::from(counts.rule_count) + SLACK as u64,
            )),
            records: sections.take(counts.records.checked_add(RECORD_PADDING as u64)),
        };
        sections.is_intact().then_some(layout)
    }

    pub(in crate::packed) fn length(&self) -> usize {
        self.records.end
    }

    pub(in crate::packed) fn header(&self) -> [u8; HEADER_BYTES] {
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

    pub(in crate::packed) fn parse(header: &[u8], length: usize) -> Option<Self> {
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

    pub(in crate::packed) fn ends_within(&self, length: usize) -> bool {
        [
            &self.positions,
            &self.piece_offsets,
            &self.pool,
            &self.entries,
            &self.shape_offsets,
            &self.shapes,
            &self.specs,
            &self.lo,
            &self.chars,
            &self.sync,
            &self.strings,
            &self.blob_sync,
            &self.blob,
            &self.template_groups,
            &self.group_starts,
            &self.rule_entries,
            &self.rules,
            &self.records,
        ]
        .iter()
        .all(|range| range.start <= range.end && range.end <= length)
    }

    pub(in crate::packed) fn moved(&self, base: usize) -> Self {
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
pub(in crate::packed) struct NumSpec {
    pub(in crate::packed) shift: u32,
    pub(in crate::packed) hi_bits: u32,
    pub(in crate::packed) lo_bits: u32,
    pub(in crate::packed) lo_start: u32,
    pub(in crate::packed) lo_count: u32,
}

impl NumSpec {
    pub(in crate::packed) fn to_bytes(self) -> [u8; SPEC_BYTES] {
        let mut bytes = [0u8; SPEC_BYTES];
        bytes[0] = self.shift as u8;
        bytes[1] = self.hi_bits as u8;
        bytes[2] = self.lo_bits as u8;
        bytes[4..8].copy_from_slice(&self.lo_start.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.lo_count.to_le_bytes());
        bytes
    }

    pub(in crate::packed) fn parse(bytes: &[u8], lo_total: u32) -> Option<Self> {
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
