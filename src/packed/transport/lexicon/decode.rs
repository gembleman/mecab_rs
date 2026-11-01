use super::{
    COMMA, HashMap, Model, NUL, NUMBER_FIELDS, Param, QUOTE, UNKNOWN_PARAM_BYTES, Undecodable,
    get_varint, visit_lexicon_lines,
};
use crate::wire::{FNV_OFFSET, fnv1a, i16_at, u16_at};
use std::borrow::Cow;

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

const EMPTY_SLOT: u32 = 0;
const FIRST_SLOT_COUNT: usize = 1 << 4;

#[derive(Default)]
pub(super) struct FeatureTable {
    slots: Vec<u32>,
    starts: Vec<usize>,
}

impl FeatureTable {
    fn bytes_of<'b>(&self, stored: &'b [u8], number: usize) -> &'b [u8] {
        let end = self.starts.get(number + 1).copied().unwrap_or(stored.len());
        &stored[self.starts[number]..end - 1]
    }

    fn slot_start(&self, feature: &[u8]) -> usize {
        fnv1a(FNV_OFFSET, feature) as usize & (self.slots.len() - 1)
    }

    fn grow_if_full(&mut self, stored: &[u8]) {
        if (self.starts.len() + 1) * 2 <= self.slots.len() {
            return;
        }
        let slot_count = (self.slots.len() * 2).max(FIRST_SLOT_COUNT);
        self.slots = vec![EMPTY_SLOT; slot_count];
        for number in 0..self.starts.len() {
            let mut at = self.slot_start(self.bytes_of(stored, number));
            while self.slots[at] != EMPTY_SLOT {
                at = (at + 1) & (slot_count - 1);
            }
            self.slots[at] = number as u32 + 1;
        }
    }

    fn intern(&mut self, stored: &mut Vec<u8>, feature: &str) -> u32 {
        self.grow_if_full(stored);
        let feature = feature.as_bytes();
        let mut at = self.slot_start(feature);
        while self.slots[at] != EMPTY_SLOT {
            let number = self.slots[at] - 1;
            if self.bytes_of(stored, number as usize) == feature {
                return number;
            }
            at = (at + 1) & (self.slots.len() - 1);
        }
        let number = self.starts.len() as u32;
        self.starts.push(stored.len());
        stored.extend_from_slice(feature);
        stored.push(NUL);
        self.slots[at] = number + 1;
        number
    }
}

#[derive(Default)]
pub(super) struct Interner {
    pub(super) params: HashMap<Param, u32>,
    pub(super) features: FeatureTable,
}

impl Interner {
    pub(super) fn intern(&mut self, model: &mut Model, param: Param, feature: &str) -> (u32, u32) {
        let next = self.params.len() as u32;
        let param_number = *self.params.entry(param).or_insert_with(|| {
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
            let (param, feature) = interner.intern(model, param, feature);
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
    let (param, feature) = interner.intern(model, param, feature);
    model.token_params.push(param);
    model.token_features.push(feature);
    match (model.keys.last(), model.counts.last_mut()) {
        (Some(last), Some(count)) if last.as_str() == key => {
            *count = count
                .checked_add(1)
                .ok_or(Undecodable("a key has more than 255 tokens"))?;
        }
        _ => {
            model.keys.push(key.into_owned());
            model.counts.push(1);
        }
    }
    Ok(())
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

pub(super) fn restore(lexicon: &[u8], unknown: &[u8], pos: &[u8]) -> Result<Model, Undecodable> {
    let unknown = Reader {
        bytes: unknown,
        at: 0,
    };
    let mut pos_ids = PosIds::new(pos);
    let mut model = Model::default();
    let mut interner = Interner::default();
    visit_lexicon_lines(lexicon, |line| {
        append_token_bytes(line, &mut pos_ids, &mut model, &mut interner)
    })?;
    decode_unknown(unknown, &mut pos_ids, &mut model, &mut interner)?;
    if !pos_ids.at_end() {
        return Err(Undecodable("the part-of-speech ids have trailing bytes"));
    }
    Ok(model)
}
