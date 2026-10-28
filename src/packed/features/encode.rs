use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use super::{
    BLOB_SYNC_EVERY, BitWriter, Counts, Layout, MAX_BLOB_FIELDS, MAX_ID_BYTES, MAX_NUMERIC,
    MAX_RULE_SUFFIX, MAX_RULES, MAX_SLOTS, NumSpec, OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_DERIVE,
    OP_EMPTY, OP_HIGH, OP_INLINE, OP_KEY, OP_NUM, OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF,
    OP_STAR, RULE_BYTES, RULE_CHAR_BITS, RULE_HEADER_BYTES, SYNC_EVERY, bits_for, chars,
    push_piece,
};
use crate::Error;

const MAX_FIELDS: usize = 256;
const FOLD_MAX_VALUES: usize = 256;
const MAX_TEMPLATES: usize = 1 << 16;
const MAX_LOW_PARTS: usize = 1 << 16;
const ABSENT: u16 = u16::MAX;
const DERIVE_TARGET: usize = 10;
const DERIVE_CONTEXT: [usize; 2] = [4, 5];
const MIN_RULE_USES: u32 = 2;
const MAX_REMOVED_CHARS: usize = 15;
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

#[derive(Clone, Copy)]
enum TextOp {
    Key,
    Star,
    Empty,
    Copy(u8),
    Derive,
    New,
}

fn text_op<'a>(value: &'a [u8], key: &[u8], derivable: bool, slots: &mut Vec<&'a [u8]>) -> TextOp {
    if is_key(value, key) {
        return TextOp::Key;
    }
    if value == b"*" {
        return TextOp::Star;
    }
    if value.is_empty() {
        return TextOp::Empty;
    }
    if let Some(slot) = slots.iter().position(|&slot| slot == value) {
        return TextOp::Copy(slot as u8);
    }
    if slots.len() < MAX_SLOTS {
        slots.push(value);
    }
    if derivable {
        TextOp::Derive
    } else {
        TextOp::New
    }
}

type Context<'a> = (&'a [u8], &'a [u8]);
type Split<'a> = (u8, &'a [u8]);
type RuleSite = (u32, char);

fn context_of<'a>(text: &'a [u8], fields: &[(usize, usize)]) -> Option<Context<'a>> {
    if fields.len() <= DERIVE_TARGET {
        return None;
    }
    let field = |number: usize| &text[fields[number].0..fields[number].1];
    Some((field(DERIVE_CONTEXT[0]), field(DERIVE_CONTEXT[1])))
}

fn split_derivation<'v>(key: &str, value: &'v [u8]) -> Option<(u8, &'v [u8])> {
    let mut common = key
        .as_bytes()
        .iter()
        .zip(value)
        .take_while(|(left, right)| left == right)
        .count();
    while !key.is_char_boundary(common) {
        common -= 1;
    }
    let removed = key[common..].chars().count();
    let suffix = &value[common..];
    let valid = removed <= MAX_REMOVED_CHARS
        && suffix.len() <= MAX_RULE_SUFFIX
        && std::str::from_utf8(suffix).is_ok();
    valid.then_some((removed as u8, suffix))
}

fn kept_bytes(key: &str, removed: u8) -> Option<usize> {
    let mut keep = key.len();
    for _ in 0..removed {
        keep -= key[..keep].chars().next_back()?.len_utf8();
    }
    Some(keep)
}

#[derive(Default)]
struct Derive<'a> {
    contexts: SmallKeyMap<Context<'a>, u32>,
    table_of_context: Vec<u16>,
    rules: SmallKeyMap<(u16, char), u16>,
    rule_list: Vec<(u8, Vec<u8>)>,
    entries: Vec<u32>,
    group_starts: Vec<u32>,
}

impl<'a> Derive<'a> {
    fn learn(
        entries: &[&'a [u8]],
        tokens: &[(u32, &str)],
        classes: &[Class],
        blob_slot: &dyn Fn(usize, usize) -> Option<usize>,
    ) -> Self {
        let enabled = classes.len() > DERIVE_TARGET
            && DERIVE_CONTEXT
                .iter()
                .all(|&field| classes[field] == Class::Folded)
            && classes[DERIVE_TARGET] == Class::Text;
        let mut derive = Self::default();
        if !enabled {
            return derive;
        }
        let mut counts: SmallKeyMap<RuleSite, SmallKeyMap<Split<'a>, u32>> = SmallKeyMap::default();
        let mut fields = Vec::new();
        for &(feature, key) in tokens {
            let entry = feature as usize;
            let Some(&text) = entries.get(entry) else {
                continue;
            };
            let Some(last) = key.chars().next_back() else {
                continue;
            };
            split_fields(text, &mut fields);
            let Some(context) = context_of(text, &fields) else {
                continue;
            };
            let next = derive.contexts.len() as u32;
            let id = *derive.contexts.entry(context).or_insert(next);
            let (begin, end) = fields[DERIVE_TARGET];
            let value = &text[begin..end];
            if blob_slot(entry, DERIVE_TARGET).is_some()
                || is_key(value, key.as_bytes())
                || value == b"*"
                || value.is_empty()
            {
                continue;
            }
            if let Some(split) = split_derivation(key, value) {
                *counts
                    .entry((id, last))
                    .or_default()
                    .entry(split)
                    .or_insert(0) += 1;
            }
        }
        let mut chosen: Vec<(RuleSite, Split)> = counts
            .into_iter()
            .filter_map(|(at, uses)| {
                let (split, count) = uses
                    .into_iter()
                    .max_by_key(|&(split, count)| (count, std::cmp::Reverse(split)))?;
                (count >= MIN_RULE_USES).then_some((at, split))
            })
            .collect();
        chosen.sort_unstable_by_key(|&(at, _)| at);
        derive.table_of_context = vec![0; derive.contexts.len()];
        let mut rule_index: SmallKeyMap<(u8, &[u8]), u16> = SmallKeyMap::default();
        let mut table = 0u16;
        let mut current: Option<u32> = None;
        for ((id, last), split) in chosen {
            let known = rule_index.get(&split).copied();
            let rule = match known {
                Some(rule) => rule,
                None if derive.rule_list.len() < MAX_RULES => {
                    let rule = derive.rule_list.len() as u16;
                    derive.rule_list.push((split.0, split.1.to_vec()));
                    rule_index.insert(split, rule);
                    rule
                }
                None => continue,
            };
            if current != Some(id) {
                current = Some(id);
                table += 1;
                derive.table_of_context[id as usize] = table;
                derive.group_starts.push(derive.entries.len() as u32);
            }
            derive.rules.insert((table, last), rule);
            derive
                .entries
                .push(last as u32 | u32::from(rule) << RULE_CHAR_BITS);
        }
        derive
    }

    fn groups(&self) -> u32 {
        self.group_starts.len() as u32
    }

    fn table_of(&self, text: &[u8], fields: &[(usize, usize)]) -> u16 {
        context_of(text, fields)
            .and_then(|context| self.contexts.get(&context))
            .map_or(0, |&id| self.table_of_context[id as usize])
    }

    fn accepts(&self, table: u16, key: &[u8], value: &[u8]) -> bool {
        if table == 0 {
            return false;
        }
        let Ok(key) = std::str::from_utf8(key) else {
            return false;
        };
        let Some(last) = key.chars().next_back() else {
            return false;
        };
        let Some(&rule) = self.rules.get(&(table, last)) else {
            return false;
        };
        let (removed, suffix) = &self.rule_list[usize::from(rule)];
        let Some(keep) = kept_bytes(key, *removed) else {
            return false;
        };
        value.len() == keep + suffix.len()
            && value[..keep] == key.as_bytes()[..keep]
            && value[keep..] == suffix[..]
    }

    fn rule_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; RULE_BYTES * self.rule_list.len()];
        for (number, (removed, suffix)) in self.rule_list.iter().enumerate() {
            let at = number * RULE_BYTES;
            bytes[at] = *removed;
            bytes[at + 1] = suffix.len() as u8;
            let start = at + RULE_HEADER_BYTES;
            bytes[start..start + suffix.len()].copy_from_slice(suffix);
        }
        bytes
    }
}

struct Planner<'p, 'a> {
    classes: &'p [Class],
    derive: &'p Derive<'a>,
    blob_slot: &'p dyn Fn(usize, usize) -> Option<usize>,
}

impl<'a> Planner<'_, 'a> {
    fn plan(
        &self,
        entry: usize,
        text: &'a [u8],
        fields: &[(usize, usize)],
        key: &[u8],
        slots: &mut Vec<&'a [u8]>,
        ops: &mut Vec<Option<TextOp>>,
    ) {
        ops.clear();
        slots.clear();
        let table = self.derive.table_of(text, fields);
        for (field, &(begin, end)) in fields.iter().enumerate() {
            if self.classes[field] != Class::Text {
                ops.push(None);
                continue;
            }
            let value = &text[begin..end];
            if (self.blob_slot)(entry, field).is_some() {
                if slots.len() < MAX_SLOTS {
                    slots.push(value);
                }
                ops.push(None);
                continue;
            }
            let derivable = field == DERIVE_TARGET && self.derive.accepts(table, key, value);
            ops.push(Some(text_op(value, key, derivable, slots)));
        }
    }

    fn uses_key(&self, ops: &[Option<TextOp>]) -> bool {
        ops.iter()
            .any(|op| matches!(op, Some(TextOp::Key | TextOp::Derive)))
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

struct Blob<'a> {
    number: usize,
    fields: Vec<usize>,
    id_bytes: usize,
    entries: Vec<(u64, Vec<&'a [u8]>)>,
    eligible: Vec<bool>,
}

impl Blob<'_> {
    fn slot(&self, entry: usize, field: usize) -> Option<usize> {
        if self.eligible[entry] {
            self.fields.iter().position(|&candidate| candidate == field)
        } else {
            None
        }
    }

    fn entry_of(&self, id: u64) -> usize {
        self.entries
            .binary_search_by_key(&id, |&(entry_id, _)| entry_id)
            .expect("every id is in the blob")
    }
}

fn blob_for_number<'a>(
    entries: &[&'a [u8]],
    field_counts: &[u16],
    classes: &[Class],
    stored: &[Stored],
    ids: &[u64],
    number: usize,
) -> Option<Blob<'a>> {
    let number_field = stored[number].field;
    let spec = stored[number].spec;
    let shift_ok = spec.shift > 0 && (1..=8 * MAX_ID_BYTES as u32).contains(&(spec.hi_bits));
    if !shift_ok {
        return None;
    }
    let texts: Vec<usize> = (0..classes.len())
        .filter(|&field| classes[field] == Class::Text && field != number_field)
        .collect();
    let mut maps: Vec<Option<SmallKeyMap<u64, &[u8]>>> =
        texts.iter().map(|_| Some(SmallKeyMap::default())).collect();
    let mut fields = Vec::new();
    for (entry, text) in entries.iter().enumerate() {
        if usize::from(field_counts[entry]) <= number_field {
            continue;
        }
        split_fields(text, &mut fields);
        for (slot, &field) in texts.iter().enumerate() {
            let (Some(map), Some(&(begin, end))) = (maps[slot].as_mut(), fields.get(field)) else {
                continue;
            };
            let value = &text[begin..end];
            if *map.entry(ids[entry]).or_insert(value) != value {
                maps[slot] = None;
            }
        }
    }
    let chosen: Vec<usize> = texts
        .iter()
        .zip(&maps)
        .filter(|(_, map)| map.as_ref().is_some_and(|map| !map.is_empty()))
        .map(|(&field, _)| field)
        .take(MAX_BLOB_FIELDS)
        .collect();
    drop(maps);
    let first = *chosen.first()?;
    if first >= number_field {
        return None;
    }
    let last = number_field.max(*chosen.last()?);
    let eligible: Vec<bool> = field_counts
        .iter()
        .map(|&count| usize::from(count) > last)
        .collect();
    let mut known: SmallKeyMap<u64, Vec<&[u8]>> = SmallKeyMap::default();
    let mut eligible_count = 0usize;
    for (entry, text) in entries.iter().enumerate() {
        if !eligible[entry] {
            continue;
        }
        eligible_count += 1;
        let id = ids[entry];
        if known.contains_key(&id) {
            continue;
        }
        split_fields(text, &mut fields);
        known.insert(
            id,
            chosen
                .iter()
                .map(|&field| &text[fields[field].0..fields[field].1])
                .collect(),
        );
    }
    if known.len() * 2 > eligible_count {
        return None;
    }
    let mut blob_entries: Vec<(u64, Vec<&[u8]>)> = known.into_iter().collect();
    blob_entries.sort_unstable_by_key(|&(id, _)| id);
    Some(Blob {
        number,
        fields: chosen,
        id_bytes: spec.hi_bits.div_ceil(8) as usize,
        entries: blob_entries,
        eligible,
    })
}

fn plan_blob<'a>(
    entries: &[&'a [u8]],
    field_counts: &[u16],
    classes: &[Class],
    stored: &[Stored],
    stored_values: &[usize],
    values: &[Vec<u64>],
) -> Option<Blob<'a>> {
    let mut best: Option<Blob<'a>> = None;
    for number in 0..stored.len() {
        let shift = stored[number].spec.shift;
        let ids: Vec<u64> = values[stored_values[number]]
            .iter()
            .map(|&value| value >> shift)
            .collect();
        let found = blob_for_number(entries, field_counts, classes, stored, &ids, number);
        if let Some(found) = found
            && best
                .as_ref()
                .is_none_or(|best| found.fields.len() > best.fields.len())
        {
            best = Some(found);
        }
    }
    best
}

struct Recoded {
    table: Vec<u64>,
    area: Vec<u8>,
    references: Vec<u32>,
    inline: Vec<Vec<u8>>,
    blob: Vec<u8>,
    blob_starts: Vec<u32>,
}

fn text_of(bytes: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(bytes).map_err(|_| Error::Unsupported("feature text that is not UTF-8"))
}

fn char_ranks(area: &[u8], inline: &[&[u8]], blob: Option<&Blob>) -> Result<Vec<char>, Error> {
    let mut counts: HashMap<char, u64> = HashMap::new();
    let blob_texts = blob.into_iter().flat_map(|blob| {
        blob.entries
            .iter()
            .flat_map(|(_, texts)| texts.iter().copied())
    });
    for bytes in area
        .split(|&byte| byte == 0)
        .chain(inline.iter().copied())
        .chain(blob_texts)
    {
        for ch in text_of(bytes)?.chars() {
            *counts.entry(ch).or_insert(0) += 1;
        }
    }
    let mut ranked: Vec<(char, u64)> = counts.into_iter().collect();
    ranked.sort_by_key(|&(ch, count)| (std::cmp::Reverse(count), ch));
    if ranked.len() > chars::MAX_CHARS {
        return Err(Error::Unsupported(
            "more distinct feature characters than codes",
        ));
    }
    Ok(ranked.into_iter().map(|(ch, _)| ch).collect())
}

fn code_text(text: &[u8], rank_of: &HashMap<char, usize>, out: &mut Vec<u8>) -> Result<(), Error> {
    for ch in text_of(text)?.chars() {
        chars::push_code(rank_of[&ch], out);
    }
    Ok(())
}

fn recode(
    area: &[u8],
    references: &[u32],
    inline: &[&[u8]],
    blob: Option<&Blob>,
) -> Result<Recoded, Error> {
    let ranked = char_ranks(area, inline, blob)?;
    let rank_of: HashMap<char, usize> = ranked
        .iter()
        .enumerate()
        .map(|(rank, &ch)| (ch, rank))
        .collect();
    let mut coded_area: Vec<u8> = Vec::new();
    let mut old_starts: Vec<u32> = Vec::new();
    let mut new_starts: Vec<u32> = Vec::new();
    let mut at = 0usize;
    while at < area.len() {
        let length = area[at..]
            .iter()
            .position(|&byte| byte == 0)
            .expect("every string of the area ends with NUL");
        old_starts.push(at as u32);
        new_starts.push(coded_area.len() as u32);
        code_text(&area[at..at + length], &rank_of, &mut coded_area)?;
        coded_area.push(0);
        at += length + 1;
    }
    let references = references
        .iter()
        .map(|offset| {
            let number = old_starts
                .binary_search(offset)
                .expect("a reference names the start of a string");
            new_starts[number]
        })
        .collect();
    let mut coded_inline = Vec::with_capacity(inline.len());
    for text in inline {
        let mut codes = Vec::new();
        code_text(text, &rank_of, &mut codes)?;
        coded_inline.push(codes);
    }
    let mut coded_blob: Vec<u8> = Vec::new();
    let mut blob_starts: Vec<u32> = Vec::new();
    if let Some(blob) = blob {
        for (id, texts) in &blob.entries {
            blob_starts.push(
                u32::try_from(coded_blob.len())
                    .map_err(|_| Error::Unsupported("feature strings of more than 4 GiB"))?,
            );
            coded_blob.extend_from_slice(&id.to_le_bytes()[..blob.id_bytes]);
            for text in texts {
                code_text(text, &rank_of, &mut coded_blob)?;
                coded_blob.push(0);
            }
        }
    }
    if coded_blob.len() > u32::MAX as usize {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }
    Ok(Recoded {
        table: ranked.into_iter().map(chars::entry_of).collect(),
        area: coded_area,
        references,
        inline: coded_inline,
        blob: coded_blob,
        blob_starts,
    })
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

    let blob = plan_blob(
        &entries,
        &field_counts,
        &classes,
        &stored,
        &stored_values,
        &values,
    );
    let blob_slot = |entry: usize, field: usize| -> Option<usize> {
        blob.as_ref().and_then(|blob| blob.slot(entry, field))
    };
    let blob_id = |entry: usize| -> u64 {
        let blob = blob.as_ref().expect("a blob field needs a blob");
        values[stored_values[blob.number]][entry] >> stored[blob.number].spec.shift
    };

    let derive = Derive::learn(&entries, tokens, &classes, &blob_slot);
    let planner = Planner {
        classes: &classes,
        derive: &derive,
        blob_slot: &blob_slot,
    };
    let mut planned: Vec<Option<TextOp>> = Vec::new();
    let mut variants: Vec<(usize, &[u8])> = Vec::new();
    let mut variant_of: SmallKeyMap<(u32, &[u8]), u32> = SmallKeyMap::default();
    let mut starts_of: Vec<u32> = Vec::with_capacity(tokens.len());
    let mut slots: Vec<&[u8]> = Vec::new();
    for &(feature, key) in tokens {
        let entry = feature as usize;
        let text = *entries.get(entry).ok_or(Error::Unsupported(
            "a token with a feature that is not there",
        ))?;
        let key = key.as_bytes();
        split_fields(text, &mut fields);
        planner.plan(entry, text, &fields, key, &mut slots, &mut planned);
        let keyed = planner.uses_key(&planned);
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
    for &(entry, key) in &variants {
        let text = entries[entry];
        split_fields(text, &mut fields);
        planner.plan(entry, text, &fields, key, &mut slots, &mut planned);
        for (&(begin, end), op) in fields.iter().zip(&planned) {
            if let Some(TextOp::New) = op {
                *string_uses.entry(&text[begin..end]).or_insert(0) += 1;
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
    let mut template_groups: Vec<u16> = Vec::new();
    let mut numbers: Vec<(u64, u32)> = Vec::new();
    let mut blob_ids: Vec<u64> = Vec::new();
    let mut program: Vec<u8> = Vec::new();
    let mut piece: Vec<u8> = Vec::new();
    let mut ops: Vec<u8> = Vec::new();
    for &(entry, key) in &variants {
        let text = entries[entry];
        split_fields(text, &mut fields);
        program.clear();
        piece.clear();
        ops.clear();
        planner.plan(entry, text, &fields, key, &mut slots, &mut planned);
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
                Class::Text if blob_slot(entry, field).is_some() => {
                    if blob_slot(entry, field) == Some(0) {
                        ops.push(OP_BLOB);
                        blob_ids.push(blob_id(entry));
                    } else {
                        ops.push(OP_BLOB_NEXT);
                    }
                }
                Class::Text => match planned[field].expect("a text field has an operation") {
                    TextOp::Key => ops.push(OP_KEY),
                    TextOp::Star => ops.push(OP_STAR),
                    TextOp::Empty => ops.push(OP_EMPTY),
                    TextOp::Copy(slot) => ops.push(OP_COPY + slot),
                    TextOp::Derive => ops.push(OP_DERIVE),
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
                    let from_blob = blob
                        .as_ref()
                        .is_some_and(|blob| blob.number == number && blob.eligible[entry]);
                    ops.push(if from_blob { OP_NUMB } else { OP_NUM } + number as u8);
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
        let template = intern_bytes(&mut templates, &mut template_list, &program);
        if template as usize == template_groups.len() {
            template_groups.push(derive.table_of(text, &fields));
        }
        entry_template.push(template);
        entry_shape.push(intern_bytes(&mut shapes, &mut shape_list, &ops));
    }
    drop(values);
    drop(known_strings);
    drop(string_uses);
    let recoded = recode(&area, &references, &inline, blob.as_ref())?;
    let (area, references, inline) = (recoded.area, recoded.references, recoded.inline);
    if area.len() > u32::MAX as usize {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }

    let template_bits = bits_for(template_list.len() as u64);
    let shape_bits = bits_for(shape_list.len() as u64);
    let ref_bits = bits_for(area.len() as u64);
    let blob_bits = bits_for(recoded.blob.len() as u64);
    let blob_offsets: Vec<u32> = blob_ids
        .iter()
        .map(|&id| {
            let blob = blob.as_ref().expect("a blob id needs a blob");
            recoded.blob_starts[blob.entry_of(id)]
        })
        .collect();
    let mut writer = BitWriter::default();
    let mut starts: Vec<u32> = Vec::with_capacity(variants.len());
    let (mut next_reference, mut next_inline, mut next_number, mut next_blob) = (0, 0, 0, 0);
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
                    writer.put_bytes(&inline[next_inline]);
                    next_inline += 1;
                }
                OP_BLOB => {
                    writer.put(u64::from(blob_offsets[next_blob]), blob_bits);
                    next_blob += 1;
                }
                OP_NUMB..=OP_NUMB_MAX => {
                    let (_, index) = numbers[next_number];
                    next_number += 1;
                    let spec = &stored[usize::from(op - OP_NUMB)].spec;
                    writer.put(u64::from(index), spec.lo_bits);
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
    if let Ok(path) = std::env::var("CHARCODE_DUMP") {
        let _ = std::fs::write(format!("{path}.area"), &area);
        let mut joined = Vec::new();
        for text in &inline {
            joined.extend_from_slice(text);
            joined.push(0);
        }
        let _ = std::fs::write(format!("{path}.inline"), &joined);
    }

    let too_big = || Error::Unsupported("a feature table that does not fit the section");
    let pieces = super::pieces::build(&template_list).ok_or_else(too_big)?;
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
        chars: recoded.table.len() as u32,
        pieces: u32::try_from(pieces.offsets.len() - 1).map_err(|_| too_big())?,
        positions: pieces.positions.len() as u32,
        entry_bits: pieces.entry_bits,
        pool_bytes: pieces.pool.len() as u64,
        shape_bytes: shape_bytes.len() as u64,
        strings: area.len() as u64,
        blob_bytes: recoded.blob.len() as u64,
        blob_entries: recoded.blob_starts.len() as u32,
        rule_groups: derive.groups(),
        rule_entries: derive.entries.len() as u32,
        rule_count: derive.rule_list.len() as u32,
        blob_fields: blob.as_ref().map_or(0, |blob| blob.fields.len() as u32),
        blob_number: blob.as_ref().map_or(0, |blob| blob.number as u32),
        id_bytes: blob.as_ref().map_or(0, |blob| blob.id_bytes as u32),
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
    let position_bytes: Vec<u8> = pieces
        .positions
        .iter()
        .flat_map(|position| position.to_bytes())
        .collect();
    put(&mut bytes, &layout.positions, &position_bytes);
    put(&mut bytes, &layout.piece_offsets, &words(&pieces.offsets));
    put(&mut bytes, &layout.pool, &pieces.pool);
    put(&mut bytes, &layout.entries, &pieces.entries);
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
    let table_bytes: Vec<u8> = recoded
        .table
        .iter()
        .flat_map(|entry| entry.to_le_bytes())
        .collect();
    put(&mut bytes, &layout.chars, &table_bytes);
    put(&mut bytes, &layout.strings, &area);
    let blob_sync: Vec<u32> = recoded
        .blob_starts
        .iter()
        .step_by(BLOB_SYNC_EVERY)
        .copied()
        .collect();
    put(&mut bytes, &layout.blob_sync, &words(&blob_sync));
    put(&mut bytes, &layout.blob, &recoded.blob);
    if derive.groups() > 0 {
        let group_bytes: Vec<u8> = template_groups
            .iter()
            .flat_map(|group| group.to_le_bytes())
            .collect();
        put(&mut bytes, &layout.template_groups, &group_bytes);
        let mut group_starts = derive.group_starts.clone();
        group_starts.push(derive.entries.len() as u32);
        put(&mut bytes, &layout.group_starts, &words(&group_starts));
        put(&mut bytes, &layout.rule_entries, &words(&derive.entries));
        put(&mut bytes, &layout.rules, &derive.rule_bytes());
    }
    put(&mut bytes, &layout.records, &records);
    let starts = starts_of
        .iter()
        .map(|&variant| starts[variant as usize])
        .collect();
    Ok(Encoded { bytes, starts })
}
