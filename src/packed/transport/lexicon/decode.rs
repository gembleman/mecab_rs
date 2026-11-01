use super::{
    COMMA, HashMap, Model, NUL, NUMBER_FIELDS, Param, QUOTE, UNKNOWN_PARAM_BYTES, Undecodable,
    get_varint, visit_lexicon_records,
};
use crate::codec::{Record, Row};
use crate::wire::{i16_at, u16_at};
use std::borrow::Cow;
use std::hash::{BuildHasherDefault, Hasher};

const PARAM_SEED: u64 = 0x517c_c1b7_2722_0a95;
const PARAM_FOLD: u32 = 32;
const DIRECT_COLS: usize = 5;
const KEY_COL: usize = 0;
const LEFT_COL: usize = 1;
const RIGHT_COL: usize = 2;
const COST_COL: usize = 3;
const FEATURE_COL: usize = 4;

#[derive(Default)]
pub(super) struct ParamHasher {
    hash: u64,
}

impl Hasher for ParamHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        let mixed = (self.hash ^ value).wrapping_mul(PARAM_SEED);
        self.hash = mixed ^ (mixed >> PARAM_FOLD);
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub(super) struct ParamKey(u64);

impl ParamKey {
    fn of(param: Param) -> Self {
        Self(
            u64::from(param.left_id)
                | (u64::from(param.right_id) << 16)
                | (u64::from(param.pos_id) << 32)
                | (u64::from(param.word_cost as u16) << 48),
        )
    }
}

type ParamMap = HashMap<ParamKey, u32, BuildHasherDefault<ParamHasher>>;

pub(super) struct Reader<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) at: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn varint(&mut self) -> Result<u64, Undecodable> {
        get_varint(self.bytes, &mut self.at).ok_or(Undecodable(
            "a varint of the unknown tokens is too long or the data ends early",
        ))
    }

    pub(super) fn count(&mut self) -> Result<usize, Undecodable> {
        usize::try_from(self.varint()?).map_err(|_| Undecodable("a count is too large"))
    }

    pub(super) fn take(&mut self, length: usize) -> Result<&'a [u8], Undecodable> {
        let end = self
            .at
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(Undecodable("the unknown tokens end early"))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    pub(super) fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

const FIRST_SLOT_COUNT: usize = 1 << 16;
const WORD_BYTES: usize = 8;
const HASH_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
const HASH_ROTATE: u32 = 23;
const HASH_SHIFT: u32 = 32;

#[derive(Clone, Copy, Default)]
struct Slot {
    number_plus_one: u32,
    hash: u32,
}

fn hash_feature(feature: &[u8]) -> u32 {
    let mut state = (feature.len() as u64).wrapping_mul(HASH_SEED);
    let (words, tail) = feature.as_chunks::<WORD_BYTES>();
    for word in words {
        let value = u64::from_le_bytes(*word);
        state = (state.rotate_left(HASH_ROTATE) ^ value).wrapping_mul(HASH_SEED);
    }
    let mut last = [0u8; WORD_BYTES];
    last[..tail.len()].copy_from_slice(tail);
    state = (state.rotate_left(HASH_ROTATE) ^ u64::from_le_bytes(last)).wrapping_mul(HASH_SEED);
    (state >> HASH_SHIFT) as u32
}

pub(super) struct FeatureTable {
    slots: Vec<Slot>,
    starts: Vec<usize>,
}

impl Default for FeatureTable {
    fn default() -> Self {
        Self {
            slots: vec![Slot::default(); FIRST_SLOT_COUNT],
            starts: Vec::new(),
        }
    }
}

impl FeatureTable {
    fn bytes_of<'b>(&self, stored: &'b [u8], number: usize) -> &'b [u8] {
        let end = self.starts.get(number + 1).copied().unwrap_or(stored.len());
        &stored[self.starts[number]..end - 1]
    }

    fn grow_if_full(&mut self) {
        if (self.starts.len() + 1) * 2 <= self.slots.len() {
            return;
        }
        let slot_count = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![Slot::default(); slot_count]);
        let mask = slot_count - 1;
        for slot in old.into_iter().filter(|slot| slot.number_plus_one != 0) {
            let mut at = slot.hash as usize & mask;
            while self.slots[at].number_plus_one != 0 {
                at = (at + 1) & mask;
            }
            self.slots[at] = slot;
        }
    }

    fn intern(&mut self, stored: &mut Vec<u8>, feature: &[u8]) -> u32 {
        self.grow_if_full();
        let hash = hash_feature(feature);
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot.number_plus_one == 0 {
                break;
            }
            let number = slot.number_plus_one - 1;
            if slot.hash == hash && self.bytes_of(stored, number as usize) == feature {
                return number;
            }
            at = (at + 1) & mask;
        }
        let number = self.starts.len() as u32;
        self.starts.push(stored.len());
        stored.extend_from_slice(feature);
        stored.push(NUL);
        self.slots[at] = Slot {
            number_plus_one: number + 1,
            hash,
        };
        number
    }
}

#[derive(Default)]
pub(super) struct Interner {
    pub(super) params: ParamMap,
    pub(super) features: FeatureTable,
}

impl Interner {
    pub(super) fn intern(&mut self, model: &mut Model, param: Param, feature: &[u8]) -> (u32, u32) {
        let next = self.params.len() as u32;
        let param_number = *self.params.entry(ParamKey::of(param)).or_insert_with(|| {
            model.params.push(param);
            next
        });
        let feature_number = self.features.intern(&mut model.features, feature);
        (param_number, feature_number)
    }
}

pub(super) struct PosIds<'a> {
    pub(super) reader: Reader<'a>,
    pub(super) current: u16,
    pub(super) left: usize,
}

impl<'a> PosIds<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self {
            reader: Reader { bytes, at: 0 },
            current: 0,
            left: 0,
        }
    }

    pub(super) fn next(&mut self) -> Result<u16, Undecodable> {
        if self.left == 0 {
            self.current = u16::try_from(self.reader.varint()?)
                .map_err(|_| Undecodable("a part-of-speech id is too large"))?;
            self.left = self.reader.count()?;
        }
        self.left = self
            .left
            .checked_sub(1)
            .ok_or(Undecodable("a part-of-speech run is empty"))?;
        Ok(self.current)
    }

    pub(super) fn at_end(&self) -> bool {
        self.left == 0 && self.reader.at_end()
    }
}

pub(super) struct CsvLine<'a> {
    pub(super) key: Cow<'a, str>,
    pub(super) param: Param,
    pub(super) feature: &'a str,
}

pub(super) fn split_key(line: &str) -> Option<(Cow<'_, str>, &str)> {
    let Some(quoted) = line.strip_prefix(QUOTE as char) else {
        let (key, rest) = line.split_once(COMMA as char)?;
        return Some((Cow::Borrowed(key), rest));
    };
    let (first, after_first) = quoted.split_once(QUOTE as char)?;
    let Some(escaped) = after_first.strip_prefix(QUOTE as char) else {
        return Some((
            Cow::Borrowed(first),
            after_first.strip_prefix(COMMA as char)?,
        ));
    };
    let mut key = String::from(first);
    key.push(QUOTE as char);
    let mut rest = escaped;
    loop {
        let (before, after) = rest.split_once(QUOTE as char)?;
        key.push_str(before);
        match after.strip_prefix(QUOTE as char) {
            Some(escaped) => {
                key.push(QUOTE as char);
                rest = escaped;
            }
            None => return Some((Cow::Owned(key), after.strip_prefix(COMMA as char)?)),
        }
    }
}

pub(super) fn parse_line(line: &str) -> Option<CsvLine<'_>> {
    let (key, rest) = split_key(line)?;
    let mut fields = rest.splitn(NUMBER_FIELDS + 1, COMMA as char);
    let left_id = fields.next()?.parse().ok()?;
    let right_id = fields.next()?.parse().ok()?;
    let word_cost = fields.next()?.parse().ok()?;
    let feature = fields.next()?;
    Some(CsvLine {
        key,
        param: Param {
            left_id,
            right_id,
            pos_id: 0,
            word_cost,
        },
        feature,
    })
}

pub(super) fn decode_unknown(
    mut reader: Reader<'_>,
    pos_ids: &mut PosIds,
    model: &mut Model,
    interner: &mut Interner,
) -> Result<(), Undecodable> {
    let categories = reader.count()?;
    let mut counts = Vec::with_capacity(categories.min(reader.bytes.len()));
    for _ in 0..categories {
        counts.push(reader.count()?);
    }
    model.unknown_starts.push(0);
    for count in counts {
        for _ in 0..count {
            let fixed = reader.take(UNKNOWN_PARAM_BYTES)?;
            let param = Param {
                left_id: u16_at(fixed, 0),
                right_id: u16_at(fixed, 2),
                pos_id: pos_ids.next()?,
                word_cost: i16_at(fixed, 4),
            };
            let length = reader.count()?;
            let feature = str::from_utf8(reader.take(length)?)
                .map_err(|_| Undecodable("an unknown feature is not valid UTF-8"))?;
            let (param, feature) = interner.intern(model, param, feature.as_bytes());
            model.unknown_params.push(param);
            model.unknown_features.push(feature);
        }
        let end = u32::try_from(model.unknown_params.len())
            .map_err(|_| Undecodable("too many unknown tokens"))?;
        model.unknown_starts.push(end);
    }
    if !reader.at_end() {
        return Err(Undecodable("the unknown tokens have trailing bytes"));
    }
    Ok(())
}

fn append_key(model: &mut Model, key: &[u8]) -> Result<(), Undecodable> {
    match (model.keys.last(), model.counts.last_mut()) {
        (Some(last), Some(count)) if last.as_bytes() == key => {
            *count = count
                .checked_add(1)
                .ok_or(Undecodable("a key has more than 255 tokens"))?;
        }
        _ => {
            let key = str::from_utf8(key).map_err(|_| Undecodable("the lexicon is not UTF-8"))?;
            model.keys.push(key.to_owned());
            model.counts.push(1);
        }
    }
    Ok(())
}

fn append_token_line(
    line: &str,
    pos_ids: &mut PosIds,
    model: &mut Model,
    interner: &mut Interner,
) -> Result<(), Undecodable> {
    let CsvLine {
        key,
        mut param,
        feature,
    } = parse_line(line).ok_or(Undecodable("the lexicon text is not valid"))?;
    param.pos_id = pos_ids.next()?;
    let (param, feature) = interner.intern(model, param, feature.as_bytes());
    model.token_params.push(param);
    model.token_features.push(feature);
    append_key(model, key.as_bytes())
}

fn append_token_bytes(
    bytes: &[u8],
    pos_ids: &mut PosIds,
    model: &mut Model,
    interner: &mut Interner,
) -> Result<(), Undecodable> {
    let line = str::from_utf8(bytes).map_err(|_| Undecodable("the lexicon is not UTF-8"))?;
    append_token_line(line, pos_ids, model, interner)
}

#[derive(Clone, Copy)]
struct DirectFields<'a> {
    key: &'a [u8],
    param: Param,
    feature: &'a [u8],
}

fn parse_number<T: TryFrom<i64>>(field: &[u8], signed: bool) -> Option<T> {
    let (negative, digits) = match field.split_first()? {
        (&b'-', rest) if signed => (true, rest),
        (&b'+', rest) => (false, rest),
        _ => (false, field),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = 0i64;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(byte - b'0');
        if value > i64::from(u32::MAX) {
            return None;
        }
    }
    T::try_from(if negative { -value } else { value }).ok()
}

fn direct_fields<'a>(row: &Row<'a>, scratch: &'a mut Vec<u8>) -> Option<DirectFields<'a>> {
    let cols = row.n_cols();
    if cols <= FEATURE_COL {
        return None;
    }
    let param = Param {
        left_id: parse_number(row.field(LEFT_COL), false)?,
        right_id: parse_number(row.field(RIGHT_COL), false)?,
        pos_id: 0,
        word_cost: parse_number(row.field(COST_COL), true)?,
    };
    let first = row.field(FEATURE_COL);
    let feature: &'a [u8] = if cols == DIRECT_COLS && is_plain(first) {
        first
    } else {
        scratch.clear();
        for col in FEATURE_COL..cols {
            if col > FEATURE_COL {
                scratch.push(COMMA);
            }
            row.write_field_csv(col, &mut |piece| scratch.extend_from_slice(piece));
        }
        scratch
    };
    Some(DirectFields {
        key: row.field(KEY_COL),
        param,
        feature,
    })
}

fn is_plain(field: &[u8]) -> bool {
    !field.iter().any(|&byte| byte == COMMA || byte == QUOTE)
}

fn append_direct(
    fields: DirectFields<'_>,
    pos_ids: &mut PosIds,
    model: &mut Model,
    interner: &mut Interner,
) -> Result<(), Undecodable> {
    let DirectFields {
        key,
        mut param,
        feature,
    } = fields;
    param.pos_id = pos_ids.next()?;
    let (param, feature) = interner.intern(model, param, feature);
    model.token_params.push(param);
    model.token_features.push(feature);
    append_key(model, key)
}

fn append_record(
    record: Record<'_>,
    scratch: &mut Vec<u8>,
    pos_ids: &mut PosIds,
    model: &mut Model,
    interner: &mut Interner,
) -> Result<(), Undecodable> {
    match record {
        Record::Irregular(bytes) => append_token_bytes(bytes, pos_ids, model, interner),
        Record::Row(row) => {
            if let Some(fields) = direct_fields(&row, scratch) {
                append_direct(fields, pos_ids, model, interner)
            } else {
                let mut line = Vec::new();
                row.write_csv(&mut |piece| line.extend_from_slice(piece));
                append_token_bytes(&line, pos_ids, model, interner)
            }
        }
    }
}

pub(super) fn restore(lexicon: &[u8], unknown: &[u8], pos: &[u8]) -> Result<Model, Undecodable> {
    let unknown = Reader {
        bytes: unknown,
        at: 0,
    };
    let mut pos_ids = PosIds::new(pos);
    let mut scratch = Vec::new();
    let mut model = Model::default();
    let mut interner = Interner::default();
    visit_lexicon_records(lexicon, |record| {
        append_record(
            record,
            &mut scratch,
            &mut pos_ids,
            &mut model,
            &mut interner,
        )
    })?;
    decode_unknown(unknown, &mut pos_ids, &mut model, &mut interner)?;
    if !pos_ids.at_end() {
        return Err(Undecodable("the part-of-speech ids have trailing bytes"));
    }
    Ok(model)
}
