use super::sizes::LexiconSizes;
use super::{
    COMMA, Model, NUL, NUMBER_FIELDS, Param, QUOTE, UNKNOWN_PARAM_BYTES, Undecodable, get_varint,
    visit_lexicon_records,
};
use crate::codec::{LexiconKeys, Record, Row};
use crate::hash::FastMap;
use crate::wire::{i16_at, u16_at};
use std::borrow::Cow;
use std::hash::BuildHasherDefault;
use std::sync::{Arc, OnceLock};

const DIRECT_COLS: usize = 5;
const KEY_COL: usize = 0;
const LEFT_COL: usize = 1;
const RIGHT_COL: usize = 2;
const COST_COL: usize = 3;
const FEATURE_COL: usize = 4;

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

const STORED_FIRST_CAPACITY: usize = 1 << 20;
const STORED_GROWTH_DIVISOR: usize = 2;

fn reserve_stored(stored: &mut Vec<u8>, extra: usize) {
    let needed = stored.len() + extra;
    let capacity = stored.capacity();
    if needed <= capacity {
        return;
    }
    let grown = capacity + capacity / STORED_GROWTH_DIVISOR;
    let target = needed.max(grown).max(STORED_FIRST_CAPACITY);
    stored.reserve_exact(target - stored.len());
}

pub(super) struct FeatureTable {
    slots: Vec<Slot>,
    starts: Vec<u32>,
}

impl Default for FeatureTable {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl FeatureTable {
    fn with_capacity(feature_count: usize) -> Self {
        let slot_count = (feature_count + 1)
            .saturating_mul(2)
            .next_power_of_two()
            .max(FIRST_SLOT_COUNT);
        Self {
            slots: vec![Slot::default(); slot_count],
            starts: Vec::with_capacity(feature_count),
        }
    }

    fn bytes_of<'b>(&self, stored: &'b [u8], number: usize) -> &'b [u8] {
        let end = self
            .starts
            .get(number + 1)
            .map_or(stored.len(), |&end| end as usize);
        &stored[self.starts[number] as usize..end - 1]
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
        debug_assert!(u32::try_from(stored.len()).is_ok());
        self.starts.push(stored.len() as u32);
        reserve_stored(stored, feature.len() + 1);
        stored.extend_from_slice(feature);
        stored.push(NUL);
        self.slots[at] = Slot {
            number_plus_one: number + 1,
            hash,
        };
        number
    }
}

type SharedKeys = Arc<OnceLock<Arc<LexiconKeys>>>;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum KeySource {
    #[default]
    Undecided,
    Own,
    Shared,
}

pub(super) struct Interner {
    pub(super) params: FastMap<ParamKey, u32>,
    pub(super) features: FeatureTable,
    key_source: KeySource,
    shared_keys: SharedKeys,
}

impl Interner {
    fn new(sizes: LexiconSizes) -> Self {
        Self {
            params: FastMap::with_capacity_and_hasher(
                sizes.param_count,
                BuildHasherDefault::default(),
            ),
            features: FeatureTable::with_capacity(sizes.feature_count),
            key_source: KeySource::default(),
            shared_keys: SharedKeys::default(),
        }
    }

    pub(super) fn intern(&mut self, model: &mut Model, param: Param, feature: &[u8]) -> (u32, u32) {
        let next = self.params.len() as u32;
        let param_number = *self.params.entry(ParamKey::of(param)).or_insert_with(|| {
            model.params.push(param);
            next
        });
        let feature_number = self.features.intern(&mut model.features, feature);
        (param_number, feature_number)
    }

    fn decide_key_source(&mut self, model: &mut Model) -> KeySource {
        if self.key_source == KeySource::Undecided {
            self.key_source = match self.shared_keys.get() {
                Some(keys) => {
                    let tokens: usize = keys.counts.iter().map(|&count| usize::from(count)).sum();
                    model.token_params.reserve_exact(tokens);
                    model.token_features.reserve_exact(tokens);
                    KeySource::Shared
                }
                None => KeySource::Own,
            };
        }
        self.key_source
    }
}

fn adopt_shared_keys(shared: SharedKeys, model: &mut Model) -> Result<(), Undecodable> {
    let keys = Arc::try_unwrap(shared)
        .ok()
        .and_then(OnceLock::into_inner)
        .ok_or(Undecodable("the decoded keys are not available"))?;
    let LexiconKeys { keys, counts } = Arc::try_unwrap(keys).unwrap_or_else(|shared| LexiconKeys {
        keys: shared.keys.clone(),
        counts: shared.counts.clone(),
    });
    let tokens: usize = counts.iter().map(|&count| usize::from(count)).sum();
    if tokens != model.token_params.len() || counts.contains(&0) {
        return Err(Undecodable("the key counts do not match the tokens"));
    }
    model.keys = keys;
    model.counts = counts;
    Ok(())
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

fn append_own_key(model: &mut Model, key: &[u8]) -> Result<(), Undecodable> {
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
        param,
        feature,
    } = parse_line(line).ok_or(Undecodable("the lexicon text is not valid"))?;
    let fields = DirectFields {
        key: key.as_bytes(),
        param,
        feature: feature.as_bytes(),
    };
    append_direct(fields, pos_ids, model, interner)
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
    let source = interner.decide_key_source(model);
    param.pos_id = pos_ids.next()?;
    let (param, feature) = interner.intern(model, param, feature);
    model.token_params.push(param);
    model.token_features.push(feature);
    match source {
        KeySource::Shared => Ok(()),
        _ => append_own_key(model, key),
    }
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

fn reserved_model(sizes: LexiconSizes) -> Model {
    let mut model = Model::default();
    model.features.reserve_exact(sizes.feature_bytes);
    model.params.reserve_exact(sizes.param_count);
    model.token_params.reserve_exact(sizes.token_count);
    model.token_features.reserve_exact(sizes.token_count);
    model
}

pub(super) fn restore(
    lexicon: &[u8],
    unknown: &[u8],
    pos: &[u8],
    sizes: &[u8],
    on_keys: impl FnOnce(Arc<LexiconKeys>) + Send,
) -> Result<Model, Undecodable> {
    let sizes = LexiconSizes::parse(sizes, lexicon.len())?;
    let unknown = Reader {
        bytes: unknown,
        at: 0,
    };
    let mut pos_ids = PosIds::new(pos);
    let mut scratch = Vec::new();
    let mut model = reserved_model(sizes);
    let mut interner = Interner::new(sizes);
    let slot = Arc::clone(&interner.shared_keys);
    let publish = move |keys: Arc<LexiconKeys>| {
        let _ = slot.set(Arc::clone(&keys));
        on_keys(keys);
    };
    visit_lexicon_records(lexicon, publish, |record| {
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
    if interner.key_source == KeySource::Shared {
        adopt_shared_keys(std::mem::take(&mut interner.shared_keys), &mut model)?;
    }
    Ok(model)
}
