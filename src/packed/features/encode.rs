use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use super::{
    BitWriter, Counts, Layout, MAX_NUMERIC, MAX_SLOTS, NumSpec, OP_COPY, OP_EMPTY, OP_HIGH,
    OP_INLINE, OP_KEY, OP_NUM, OP_NUM_MAX, OP_REF, OP_STAR, SYNC_EVERY, bits_for, push_piece,
};
use crate::Error;

const MAX_FIELDS: usize = 256;
const FOLD_MAX_VALUES: usize = 256;
const MAX_TEMPLATES: usize = 1 << 16;
const MAX_LOW_PARTS: usize = 1 << 16;
const ABSENT: u16 = u16::MAX;
const HASH_MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;
const FINAL_MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

pub(in crate::packed) struct Encoded {
    pub(in crate::packed) bytes: Vec<u8>,
    pub(in crate::packed) starts: Vec<u32>,
}

fn is_key(value: &[u8], key: &[u8]) -> bool {
    !key.is_empty() && value == key && value != b"*"
}

#[derive(Default)]
struct SmallKeyHasher(u64);

impl SmallKeyHasher {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(HASH_MULTIPLIER);
    }
}

impl Hasher for SmallKeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for &chunk in chunks {
            self.add(u64::from_le_bytes(chunk));
        }
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        self.add(u64::from_le_bytes(last) ^ (rest.len() as u64) << 59);
    }

    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    fn write_u16(&mut self, value: u16) {
        self.add(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    fn finish(&self) -> u64 {
        let mut hash = self.0;
        hash ^= hash >> 32;
        hash = hash.wrapping_mul(FINAL_MULTIPLIER);
        hash ^ (hash >> 29)
    }
}

type SmallKeyMap<K, V> = HashMap<K, V, BuildHasherDefault<SmallKeyHasher>>;
type SmallKeySet<K> = HashSet<K, BuildHasherDefault<SmallKeyHasher>>;

pub(in crate::packed) fn split_fields(text: &[u8], fields: &mut Vec<(usize, usize)>) {
    fields.clear();
    let length = text.len();
    let mut start = 0;
    loop {
        if fields.len() == MAX_FIELDS - 1 {
            fields.push((start, length));
            return;
        }
        let mut end = None;
        if text.get(start) == Some(&b'"') {
            let mut at = start + 1;
            while let Some(found) = text[at..].iter().position(|&byte| byte == b'"') {
                let quote = at + found;
                if text.get(quote + 1) == Some(&b'"') {
                    at = quote + 2;
                    continue;
                }
                if quote + 1 == length || text[quote + 1] == b',' {
                    end = Some(quote + 1);
                }
                break;
            }
        }
        let end = end.unwrap_or_else(|| {
            text[start..]
                .iter()
                .position(|&byte| byte == b',')
                .map_or(length, |found| start + found)
        });
        fields.push((start, end));
        if end == length {
            return;
        }
        start = end + 1;
    }
}

fn parse_number(field: &[u8]) -> Option<u64> {
    if field.is_empty() || field.len() > 20 || (field[0] == b'0' && field.len() > 1) {
        return None;
    }
    let mut value = 0u64;
    for &byte in field {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

struct Stat<'a> {
    present: usize,
    values: SmallKeyMap<&'a [u8], u16>,
    value_ids: Vec<u16>,
    overflow: bool,
    numeric: bool,
}

impl<'a> Stat<'a> {
    fn new(entries: usize) -> Self {
        Self {
            present: 0,
            values: SmallKeyMap::default(),
            value_ids: vec![ABSENT; entries],
            overflow: false,
            numeric: true,
        }
    }

    fn observe(&mut self, entry: usize, value: &'a [u8]) {
        self.present += 1;
        if self.numeric && parse_number(value).is_none() {
            self.numeric = false;
        }
        if self.overflow {
            return;
        }
        if let Some(&id) = self.values.get(value) {
            self.value_ids[entry] = id;
        } else if self.values.len() >= FOLD_MAX_VALUES {
            self.overflow = true;
            self.values = SmallKeyMap::default();
            self.value_ids = Vec::new();
        } else {
            let id = self.values.len() as u16;
            self.values.insert(value, id);
            self.value_ids[entry] = id;
        }
    }

    fn folds(&self) -> bool {
        !self.overflow
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Folded,
    Text,
    Number(usize),
    High(usize),
}

fn choose_split(values: &[u64], preferred_shift: Option<u32>) -> (u32, Vec<u64>) {
    let maximum = values.iter().copied().max().unwrap_or(0);
    let top = 64 - maximum.leading_zeros();
    let mut best = (f64::MAX, 0u32);
    let mut lows: SmallKeySet<u64> = SmallKeySet::default();
    for shift in 0..top.max(1) {
        let (hi_bits, count) = if shift == 0 {
            (top, 1)
        } else {
            lows.clear();
            let mask = (1u64 << shift) - 1;
            for &value in values {
                lows.insert(value & mask);
                if lows.len() > MAX_LOW_PARTS {
                    break;
                }
            }
            (top - shift, lows.len())
        };
        if count > MAX_LOW_PARTS {
            break;
        }
        let cost = f64::from(hi_bits)
            + f64::from(bits_for(count as u64))
            + 64.0 * count as f64 / values.len().max(1) as f64;
        if cost < best.0 {
            best = (cost, shift);
        }
    }
    let mut shift = best.1;
    if let Some(preferred_shift) = preferred_shift {
        let mask = (1u64 << preferred_shift) - 1;
        let mut set: SmallKeySet<u64> = SmallKeySet::default();
        set.extend(values.iter().map(|&value| value & mask));
        if set.len() <= MAX_LOW_PARTS {
            shift = preferred_shift;
        }
    }
    let mut table: Vec<u64> = if shift == 0 {
        vec![0]
    } else {
        let mask = (1u64 << shift) - 1;
        let mut set: SmallKeySet<u64> = SmallKeySet::default();
        set.extend(values.iter().map(|&value| value & mask));
        set.into_iter().collect()
    };
    table.sort_unstable();
    (shift, table)
}

enum TextOp {
    Star,
    Empty,
    Copy(u8),
    New,
}

fn text_op<'a>(value: &'a [u8], slots: &mut Vec<&'a [u8]>) -> TextOp {
    if value == b"*" {
        TextOp::Star
    } else if value.is_empty() {
        TextOp::Empty
    } else if let Some(slot) = slots.iter().position(|&slot| slot == value) {
        TextOp::Copy(slot as u8)
    } else {
        if slots.len() < MAX_SLOTS {
            slots.push(value);
        }
        TextOp::New
    }
}

fn intern_bytes(map: &mut SmallKeyMap<Vec<u8>, u32>, list: &mut Vec<Vec<u8>>, bytes: &[u8]) -> u32 {
    if let Some(&number) = map.get(bytes) {
        return number;
    }
    let number = list.len() as u32;
    list.push(bytes.to_vec());
    map.insert(bytes.to_vec(), number);
    number
}

fn shifted_copy(
    earlier: &[u64],
    later: &[u64],
    field_counts: &[u16],
    first_field: usize,
    later_field: usize,
) -> Option<u32> {
    let has_both = |entry: usize| usize::from(field_counts[entry]) > first_field.max(later_field);
    let sample = (0..earlier.len()).find(|&entry| has_both(entry) && earlier[entry] != 0)?;
    (1..64u32).find(|&shift| {
        earlier[sample] >> shift == later[sample]
            && (0..earlier.len()).all(|entry| {
                usize::from(field_counts[entry]) <= later_field
                    || (has_both(entry) && later[entry] == earlier[entry] >> shift)
            })
    })
}

struct Stored {
    field: usize,
    spec: NumSpec,
    low_table: Vec<u64>,
}

pub(in crate::packed) fn encode(features: &[u8], tokens: &[(u32, &str)]) -> Result<Encoded, Error> {
    let mut entries: Vec<&[u8]> = features.split(|&byte| byte == 0).collect();
    entries.pop();
    let count = entries.len();
    if u32::try_from(count).is_err() {
        return Err(Error::Unsupported("more than 2^32 feature strings"));
    }

    let mut stats: Vec<Stat> = Vec::new();
    let mut field_counts: Vec<u16> = Vec::with_capacity(count);
    let mut fields = Vec::new();
    for (entry, text) in entries.iter().enumerate() {
        split_fields(text, &mut fields);
        field_counts.push(fields.len() as u16);
        while stats.len() < fields.len() {
            stats.push(Stat::new(count));
        }
        for (field, &(begin, end)) in fields.iter().enumerate() {
            stats[field].observe(entry, &text[begin..end]);
        }
    }

    let mut joint: Vec<u32> = {
        let mut known: SmallKeyMap<u16, u32> = SmallKeyMap::default();
        field_counts
            .iter()
            .map(|&number| {
                let next = known.len() as u32;
                *known.entry(number).or_insert(next)
            })
            .collect()
    };
    let mut order: Vec<usize> = (0..stats.len()).filter(|&f| stats[f].folds()).collect();
    order.sort_by_key(|&field| (stats[field].values.len(), field));
    let mut classes = vec![Class::Text; stats.len()];
    for field in order {
        let mut known: SmallKeyMap<(u32, u16), u32> = SmallKeyMap::default();
        let mut next = Vec::with_capacity(count);
        for (entry, &previous) in joint.iter().enumerate() {
            let length = known.len() as u32;
            next.push(
                *known
                    .entry((previous, stats[field].value_ids[entry]))
                    .or_insert(length),
            );
            if known.len() > MAX_TEMPLATES {
                break;
            }
        }
        if known.len() <= MAX_TEMPLATES {
            joint = next;
            classes[field] = Class::Folded;
        }
    }
    drop(joint);
    for stat in &mut stats {
        stat.value_ids = Vec::new();
    }

    let candidates: Vec<usize> = (0..stats.len())
        .filter(|&f| classes[f] == Class::Text && stats[f].numeric && stats[f].present > 0)
        .collect();
    let mut values: Vec<Vec<u64>> = vec![vec![0; count]; candidates.len()];
    for (entry, text) in entries.iter().enumerate() {
        split_fields(text, &mut fields);
        for (slot, &field) in candidates.iter().enumerate() {
            if let Some(&(begin, end)) = fields.get(field) {
                values[slot][entry] = parse_number(&text[begin..end]).unwrap_or(0);
            }
        }
    }
    let mut stored: Vec<Stored> = Vec::new();
    let mut stored_values: Vec<usize> = Vec::new();
    let mut low_total = 0usize;
    for (slot, &field) in candidates.iter().enumerate() {
        let has = |entry: usize| usize::from(field_counts[entry]) > field;
        let derived = stored.iter().enumerate().position(|(number, source)| {
            let shift = source.spec.shift;
            (0..count).all(|entry| {
                !has(entry)
                    || (usize::from(field_counts[entry]) > source.field
                        && values[slot][entry] == values[stored_values[number]][entry] >> shift)
            })
        });
        if let Some(number) = derived {
            classes[field] = Class::High(number);
            continue;
        }
        if stored.len() == MAX_NUMERIC {
            continue;
        }
        let present: Vec<u64> = (0..count)
            .filter(|&entry| has(entry))
            .map(|entry| values[slot][entry])
            .collect();
        let preferred_shift =
            candidates
                .iter()
                .enumerate()
                .skip(slot + 1)
                .find_map(|(other, &later)| {
                    shifted_copy(&values[slot], &values[other], &field_counts, field, later)
                });
        let (shift, low_table) = choose_split(&present, preferred_shift);
        let maximum = present.iter().copied().max().unwrap_or(0);
        let hi_bits = 64 - (maximum >> shift).leading_zeros();
        let spec = NumSpec {
            shift,
            hi_bits,
            lo_bits: bits_for(low_table.len() as u64),
            lo_start: low_total as u32,
            lo_count: low_table.len() as u32,
        };
        low_total += low_table.len();
        classes[field] = Class::Number(stored.len());
        stored.push(Stored {
            field,
            spec,
            low_table,
        });
        stored_values.push(slot);
    }

    let mut variants: Vec<(usize, &[u8])> = Vec::new();
    let mut variant_of: SmallKeyMap<(u32, &[u8]), u32> = SmallKeyMap::default();
    let mut starts_of: Vec<u32> = Vec::with_capacity(tokens.len());
    for &(feature, key) in tokens {
        let entry = feature as usize;
        let text = *entries.get(entry).ok_or(Error::Unsupported(
            "a token with a feature that is not there",
        ))?;
        let key = key.as_bytes();
        split_fields(text, &mut fields);
        let keyed = fields.iter().enumerate().any(|(field, &(begin, end))| {
            classes[field] == Class::Text && is_key(&text[begin..end], key)
        });
        let key: &[u8] = if keyed { key } else { b"" };
        let next = variants.len() as u32;
        let number = *variant_of.entry((feature, key)).or_insert(next);
        if number == next {
            variants.push((entry, key));
        }
        starts_of.push(number);
    }
    drop(variant_of);

    let mut string_uses: SmallKeyMap<&[u8], u32> = SmallKeyMap::default();
    let mut slots: Vec<&[u8]> = Vec::new();
    for &(entry, key) in &variants {
        let text = entries[entry];
        split_fields(text, &mut fields);
        slots.clear();
        for (field, &(begin, end)) in fields.iter().enumerate() {
            if classes[field] == Class::Text {
                let value = &text[begin..end];
                if is_key(value, key) {
                    continue;
                }
                if let TextOp::New = text_op(value, &mut slots) {
                    *string_uses.entry(value).or_insert(0) += 1;
                }
            }
        }
    }

    let mut templates: SmallKeyMap<Vec<u8>, u32> = SmallKeyMap::default();
    let mut template_list: Vec<Vec<u8>> = Vec::new();
    let mut shapes: SmallKeyMap<Vec<u8>, u32> = SmallKeyMap::default();
    let mut shape_list: Vec<Vec<u8>> = Vec::new();
    let mut known_strings: SmallKeyMap<&[u8], u32> = SmallKeyMap::default();
    let mut area: Vec<u8> = Vec::new();
    let mut entry_template: Vec<u32> = Vec::with_capacity(variants.len());
    let mut entry_shape: Vec<u32> = Vec::with_capacity(variants.len());
    let mut references: Vec<u32> = Vec::new();
    let mut inline: Vec<&[u8]> = Vec::new();
    let mut numbers: Vec<(u64, u32)> = Vec::new();
    let mut program: Vec<u8> = Vec::new();
    let mut piece: Vec<u8> = Vec::new();
    let mut ops: Vec<u8> = Vec::new();
    for &(entry, key) in &variants {
        let text = entries[entry];
        split_fields(text, &mut fields);
        program.clear();
        piece.clear();
        ops.clear();
        slots.clear();
        for (field, &(begin, end)) in fields.iter().enumerate() {
            if field > 0 {
                piece.push(b',');
            }
            let value = &text[begin..end];
            if classes[field] == Class::Folded {
                piece.extend_from_slice(value);
                continue;
            }
            push_piece(&mut program, &piece);
            piece.clear();
            match classes[field] {
                Class::Folded => {}
                Class::Text if is_key(value, key) => ops.push(OP_KEY),
                Class::Text => match text_op(value, &mut slots) {
                    TextOp::Star => ops.push(OP_STAR),
                    TextOp::Empty => ops.push(OP_EMPTY),
                    TextOp::Copy(slot) => ops.push(OP_COPY + slot),
                    TextOp::New if string_uses[value] == 1 => {
                        ops.push(OP_INLINE);
                        inline.push(value);
                    }
                    TextOp::New => {
                        ops.push(OP_REF);
                        let next = area.len();
                        let offset = *known_strings.entry(value).or_insert_with(|| next as u32);
                        if offset as usize == next {
                            area.extend_from_slice(value);
                            area.push(0);
                        }
                        references.push(offset);
                    }
                },
                Class::Number(number) => {
                    ops.push(OP_NUM + number as u8);
                    let spec = &stored[number];
                    let value = values[stored_values[number]][entry];
                    let low = value & ((1u64 << spec.spec.shift) - 1);
                    let index = spec
                        .low_table
                        .binary_search(&low)
                        .expect("every low part is in the table");
                    numbers.push((value >> spec.spec.shift, index as u32));
                }
                Class::High(number) => ops.push(OP_HIGH + number as u8),
            }
        }
        push_piece(&mut program, &piece);
        if area.len() > u32::MAX as usize {
            return Err(Error::Unsupported("feature strings of more than 4 GiB"));
        }
        entry_template.push(intern_bytes(&mut templates, &mut template_list, &program));
        entry_shape.push(intern_bytes(&mut shapes, &mut shape_list, &ops));
    }
    drop(values);
    drop(known_strings);
    drop(string_uses);

    let template_bits = bits_for(template_list.len() as u64);
    let shape_bits = bits_for(shape_list.len() as u64);
    let ref_bits = bits_for(area.len() as u64);
    let mut writer = BitWriter::default();
    let mut starts: Vec<u32> = Vec::with_capacity(variants.len());
    let (mut next_reference, mut next_inline, mut next_number) = (0, 0, 0);
    for entry in 0..variants.len() {
        let start = writer.bytes.len();
        starts.push(
            u32::try_from(start)
                .map_err(|_| Error::Unsupported("feature records of more than 4 GiB"))?,
        );
        writer.put(u64::from(entry_template[entry]), template_bits);
        writer.put(u64::from(entry_shape[entry]), shape_bits);
        for &op in &shape_list[entry_shape[entry] as usize] {
            match op {
                OP_REF => {
                    writer.put(u64::from(references[next_reference]), ref_bits);
                    next_reference += 1;
                }
                OP_INLINE => {
                    writer.put_bytes(inline[next_inline]);
                    next_inline += 1;
                }
                OP_NUM..=OP_NUM_MAX => {
                    let (high, index) = numbers[next_number];
                    next_number += 1;
                    let spec = &stored[usize::from(op - OP_NUM)].spec;
                    writer.put(high, spec.hi_bits);
                    writer.put(u64::from(index), spec.lo_bits);
                }
                _ => {}
            }
        }
        writer.finish_record();
        if writer.bytes.len() == start {
            writer.bytes.push(0);
        }
    }
    let records = writer.bytes;

    let too_big = || Error::Unsupported("a feature table that does not fit the section");
    let mut template_offsets: Vec<u32> = vec![0];
    let mut template_bytes: Vec<u8> = Vec::new();
    for template in &template_list {
        template_bytes.extend_from_slice(template);
        template_offsets.push(u32::try_from(template_bytes.len()).map_err(|_| too_big())?);
    }
    let mut shape_offsets: Vec<u32> = vec![0];
    let mut shape_bytes: Vec<u8> = Vec::new();
    for shape in &shape_list {
        shape_bytes.extend_from_slice(shape);
        shape_offsets.push(u32::try_from(shape_bytes.len()).map_err(|_| too_big())?);
    }
    let low_values: Vec<u64> = stored
        .iter()
        .flat_map(|source| source.low_table.iter().copied())
        .collect();
    let counts = Counts {
        templates: template_list.len() as u32,
        shapes: shape_list.len() as u32,
        numerics: stored.len() as u32,
        lo_total: u32::try_from(low_values.len()).map_err(|_| too_big())?,
        template_bytes: template_bytes.len() as u64,
        shape_bytes: shape_bytes.len() as u64,
        strings: area.len() as u64,
        records: records.len() as u64,
        record_count: variants.len() as u32,
    };
    let layout = Layout::new(counts).ok_or_else(too_big)?;
    let mut bytes = vec![0u8; layout.length()];
    bytes[..super::HEADER_BYTES].copy_from_slice(&layout.header());
    let put = |bytes: &mut Vec<u8>, range: &std::ops::Range<usize>, data: &[u8]| {
        bytes[range.start..range.start + data.len()].copy_from_slice(data);
    };
    let words =
        |values: &[u32]| -> Vec<u8> { values.iter().flat_map(|v| v.to_le_bytes()).collect() };
    put(
        &mut bytes,
        &layout.template_offsets,
        &words(&template_offsets),
    );
    put(&mut bytes, &layout.templates, &template_bytes);
    put(&mut bytes, &layout.shape_offsets, &words(&shape_offsets));
    put(&mut bytes, &layout.shapes, &shape_bytes);
    let spec_bytes: Vec<u8> = stored
        .iter()
        .flat_map(|source| source.spec.to_bytes())
        .collect();
    put(&mut bytes, &layout.specs, &spec_bytes);
    let low_bytes: Vec<u8> = low_values.iter().flat_map(|v| v.to_le_bytes()).collect();
    put(&mut bytes, &layout.lo, &low_bytes);
    let sync: Vec<u32> = starts.iter().step_by(SYNC_EVERY).copied().collect();
    put(&mut bytes, &layout.sync, &words(&sync));
    put(&mut bytes, &layout.strings, &area);
    put(&mut bytes, &layout.records, &records);
    let starts = starts_of
        .iter()
        .map(|&variant| starts[variant as usize])
        .collect();
    Ok(Encoded { bytes, starts })
}
