use std::collections::HashMap;
use std::io::Write;

use super::{Part, Parts};
use crate::Error;
use crate::codec::{decode_lexicon, encode_lexicon};
use crate::packed::model::{Model, Param};

const ENCODING_PLAIN: u32 = 0;
const ENCODING_CODEC: u32 = 2;
const LEXICON_PART: &str = "lexicon";
const UNKNOWN_PART: &str = "unknown tokens";
const POS_PART: &str = "pos ids";
const NUL: u8 = 0;
const COMMA: u8 = b',';
const QUOTE: u8 = b'"';
const LINE_END: u8 = b'\n';
const UNKNOWN_PARAM_BYTES: usize = 6;
const NUMBER_FIELDS: usize = 3;

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, Error> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self
                .bytes
                .get(self.at)
                .ok_or(Error::Corrupt("the unknown tokens end early"))?;
            self.at += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::Corrupt("a varint of the unknown tokens is too long"))
    }

    fn count(&mut self) -> Result<usize, Error> {
        usize::try_from(self.varint()?).map_err(|_| Error::Corrupt("a count is too large"))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let end = self
            .at
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(Error::Corrupt("the unknown tokens end early"))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

struct Interner<'a> {
    params: HashMap<Param, u32>,
    features: HashMap<&'a str, u32>,
}

impl<'a> Interner<'a> {
    fn intern(&mut self, model: &mut Model, param: Param, feature: &'a str) -> (u32, u32) {
        let next = self.params.len() as u32;
        let param_number = *self.params.entry(param).or_insert_with(|| {
            model.params.push(param);
            next
        });
        let next = self.features.len() as u32;
        let feature_number = *self.features.entry(feature).or_insert_with(|| {
            model.features.extend_from_slice(feature.as_bytes());
            model.features.push(NUL);
            next
        });
        (param_number, feature_number)
    }
}

fn check_encodable(model: &Model) -> Result<(), Error> {
    let counted: usize = model.counts.iter().map(|&count| usize::from(count)).sum();
    if model.counts.len() != model.keys.len()
        || counted != model.token_params.len()
        || counted != model.token_features.len()
        || model.unknown_params.len() != model.unknown_features.len()
    {
        return Err(Error::Unsupported(
            "token counts that do not match the tokens",
        ));
    }
    if model.keys.iter().any(|key| key.contains(LINE_END as char)) {
        return Err(Error::Unsupported("a key with a line break"));
    }
    if model.features.contains(&LINE_END) {
        return Err(Error::Unsupported("a feature with a line break"));
    }
    Ok(())
}

fn feature_table(model: &Model) -> Vec<&[u8]> {
    let mut features: Vec<&[u8]> = model.features.split(|&byte| byte == NUL).collect();
    features.pop();
    features
}

fn push_key(csv: &mut Vec<u8>, key: &str) {
    if key.bytes().any(|byte| byte == COMMA || byte == QUOTE) {
        csv.push(QUOTE);
        for byte in key.bytes() {
            if byte == QUOTE {
                csv.push(QUOTE);
            }
            csv.push(byte);
        }
        csv.push(QUOTE);
    } else {
        csv.extend_from_slice(key.as_bytes());
    }
}

fn model_to_csv(model: &Model) -> Result<Vec<u8>, Error> {
    let features = feature_table(model);
    let in_range = |numbers: &[u32], limit: usize| numbers.iter().all(|&n| (n as usize) < limit);
    if !in_range(&model.token_params, model.params.len())
        || !in_range(&model.unknown_params, model.params.len())
        || !in_range(&model.token_features, features.len())
        || !in_range(&model.unknown_features, features.len())
    {
        return Err(Error::Unsupported("a token number out of range"));
    }
    let mut csv = Vec::with_capacity(model.features.len() * 8);
    let mut token = 0usize;
    for (key, &count) in model.keys.iter().zip(&model.counts) {
        for _ in 0..count {
            let param = model.params[model.token_params[token] as usize];
            push_key(&mut csv, key);
            write!(
                csv,
                ",{},{},{},",
                param.left_id, param.right_id, param.word_cost
            )
            .expect("a vector accepts writes");
            csv.extend_from_slice(features[model.token_features[token] as usize]);
            csv.push(LINE_END);
            token += 1;
        }
    }
    Ok(csv)
}

fn pos_runs(model: &Model) -> Vec<u8> {
    let tokens = model.token_params.iter().chain(&model.unknown_params);
    let mut out = Vec::new();
    let mut run: Option<(u16, u64)> = None;
    for &param in tokens {
        let pos_id = model.params[param as usize].pos_id;
        run = match run {
            Some((current, length)) if current == pos_id => Some((current, length + 1)),
            Some((current, length)) => {
                put_varint(&mut out, u64::from(current));
                put_varint(&mut out, length);
                Some((pos_id, 1))
            }
            None => Some((pos_id, 1)),
        };
    }
    if let Some((current, length)) = run {
        put_varint(&mut out, u64::from(current));
        put_varint(&mut out, length);
    }
    out
}

struct PosIds<'a> {
    reader: Reader<'a>,
    current: u16,
    left: usize,
}

impl<'a> PosIds<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            reader: Reader { bytes, at: 0 },
            current: 0,
            left: 0,
        }
    }

    fn next(&mut self) -> Result<u16, Error> {
        if self.left == 0 {
            self.current = u16::try_from(self.reader.varint()?)
                .map_err(|_| Error::Corrupt("a part-of-speech id is too large"))?;
            self.left = self.reader.count()?;
        }
        self.left = self
            .left
            .checked_sub(1)
            .ok_or(Error::Corrupt("a part-of-speech run is empty"))?;
        Ok(self.current)
    }

    fn at_end(&self) -> bool {
        self.left == 0 && self.reader.at_end()
    }
}

fn unknown_tokens(model: &Model) -> Vec<u8> {
    let features = feature_table(model);
    let mut out = Vec::new();
    let categories = model.unknown_starts.len().saturating_sub(1);
    put_varint(&mut out, categories as u64);
    for pair in model.unknown_starts.windows(2) {
        put_varint(&mut out, u64::from(pair[1] - pair[0]));
    }
    for (&param, &feature) in model.unknown_params.iter().zip(&model.unknown_features) {
        let param = model.params[param as usize];
        out.extend_from_slice(&param.left_id.to_le_bytes());
        out.extend_from_slice(&param.right_id.to_le_bytes());
        out.extend_from_slice(&param.word_cost.to_le_bytes());
        let feature = features[feature as usize];
        put_varint(&mut out, feature.len() as u64);
        out.extend_from_slice(feature);
    }
    out
}

struct CsvLine<'a> {
    key: String,
    param: Param,
    feature: &'a str,
}

fn split_key(line: &str) -> Option<(String, &str)> {
    let Some(quoted) = line.strip_prefix(QUOTE as char) else {
        let (key, rest) = line.split_once(COMMA as char)?;
        return Some((key.to_string(), rest));
    };
    let mut key = String::new();
    let mut rest = quoted;
    loop {
        let (before, after) = rest.split_once(QUOTE as char)?;
        key.push_str(before);
        match after.strip_prefix(QUOTE as char) {
            Some(escaped) => {
                key.push(QUOTE as char);
                rest = escaped;
            }
            None => return Some((key, after.strip_prefix(COMMA as char)?)),
        }
    }
}

fn parse_line(line: &str) -> Option<CsvLine<'_>> {
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

fn decode_unknown<'a>(
    mut reader: Reader<'a>,
    pos_ids: &mut PosIds,
    model: &mut Model,
    interner: &mut Interner<'a>,
) -> Result<(), Error> {
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
                left_id: u16::from_le_bytes([fixed[0], fixed[1]]),
                right_id: u16::from_le_bytes([fixed[2], fixed[3]]),
                pos_id: pos_ids.next()?,
                word_cost: i16::from_le_bytes([fixed[4], fixed[5]]),
            };
            let length = reader.count()?;
            let feature = str::from_utf8(reader.take(length)?)
                .map_err(|_| Error::Corrupt("an unknown feature is not valid UTF-8"))?;
            let (param, feature) = interner.intern(model, param, feature);
            model.unknown_params.push(param);
            model.unknown_features.push(feature);
        }
        let end = u32::try_from(model.unknown_params.len())
            .map_err(|_| Error::Corrupt("too many unknown tokens"))?;
        model.unknown_starts.push(end);
    }
    if !reader.at_end() {
        return Err(Error::Corrupt("the unknown tokens have trailing bytes"));
    }
    Ok(())
}

fn model_from_csv(csv: &[u8], unknown: &[u8], pos: &[u8]) -> Result<Model, Error> {
    let text = str::from_utf8(csv).map_err(|_| Error::Corrupt("the lexicon is not UTF-8"))?;
    let unknown = Reader {
        bytes: unknown,
        at: 0,
    };
    let mut pos_ids = PosIds::new(pos);
    let mut model = Model::default();
    let mut interner = Interner {
        params: HashMap::new(),
        features: HashMap::new(),
    };
    for line in text.split_terminator(LINE_END as char) {
        let CsvLine {
            key,
            mut param,
            feature,
        } = parse_line(line).ok_or(Error::Corrupt("the lexicon text is not valid"))?;
        param.pos_id = pos_ids.next()?;
        let (param, feature) = interner.intern(&mut model, param, feature);
        model.token_params.push(param);
        model.token_features.push(feature);
        match (model.keys.last(), model.counts.last_mut()) {
            (Some(last), Some(count)) if *last == key => {
                *count = count
                    .checked_add(1)
                    .ok_or(Error::Corrupt("a key has more than 255 tokens"))?;
            }
            _ => {
                model.keys.push(key);
                model.counts.push(1);
            }
        }
    }
    decode_unknown(unknown, &mut pos_ids, &mut model, &mut interner)?;
    if !pos_ids.at_end() {
        return Err(Error::Corrupt("the part-of-speech ids have trailing bytes"));
    }
    Ok(model)
}

fn same_lexicon(a: &Model, b: &Model) -> bool {
    a.keys == b.keys
        && a.counts == b.counts
        && a.token_params == b.token_params
        && a.token_features == b.token_features
        && a.params == b.params
        && a.features == b.features
        && a.unknown_starts == b.unknown_starts
        && a.unknown_params == b.unknown_params
        && a.unknown_features == b.unknown_features
}

fn restore(lexicon: &[u8], unknown: &[u8], pos: &[u8]) -> Result<Model, Error> {
    model_from_csv(&decode_lexicon(lexicon)?, unknown, pos)
}

pub(super) fn encode(model: &Model) -> Result<Vec<Part>, Error> {
    check_encodable(model)?;
    let lexicon = encode_lexicon(&model_to_csv(model)?);
    let (unknown, pos) = (unknown_tokens(model), pos_runs(model));
    if !same_lexicon(&restore(&lexicon, &unknown, &pos)?, model) {
        return Err(Error::Unsupported(
            "a lexicon that the codec form does not restore exactly",
        ));
    }
    Ok(vec![
        Part {
            name: LEXICON_PART,
            encoding: ENCODING_CODEC,
            bytes: lexicon,
        },
        Part {
            name: UNKNOWN_PART,
            encoding: ENCODING_PLAIN,
            bytes: unknown,
        },
        Part {
            name: POS_PART,
            encoding: ENCODING_PLAIN,
            bytes: pos,
        },
    ])
}

pub(super) fn decode(parts: &Parts) -> Result<Model, Error> {
    let lexicon = parts.plain(LEXICON_PART, ENCODING_CODEC)?;
    let unknown = parts.plain(UNKNOWN_PART, ENCODING_PLAIN)?;
    let pos = parts.plain(POS_PART, ENCODING_PLAIN)?;
    restore(lexicon, unknown, pos)
}
