use super::{
    COMMA, Error, HashMap, LINE_END, Model, NUL, NUMBER_FIELDS, Param, QUOTE, UNKNOWN_PARAM_BYTES,
    decode_lexicon, get_varint,
};
use crate::wire::{i16_at, u16_at};

pub(super) struct Reader<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) at: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn varint(&mut self) -> Result<u64, Error> {
        get_varint(self.bytes, &mut self.at).ok_or(Error::Corrupt(
            "a varint of the unknown tokens is too long or the data ends early",
        ))
    }

    pub(super) fn count(&mut self) -> Result<usize, Error> {
        usize::try_from(self.varint()?).map_err(|_| Error::Corrupt("a count is too large"))
    }

    pub(super) fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let end = self
            .at
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(Error::Corrupt("the unknown tokens end early"))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    pub(super) fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

pub(super) struct Interner<'a> {
    pub(super) params: HashMap<Param, u32>,
    pub(super) features: HashMap<&'a str, u32>,
}

impl<'a> Interner<'a> {
    pub(super) fn intern(
        &mut self,
        model: &mut Model,
        param: Param,
        feature: &'a str,
    ) -> (u32, u32) {
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

    pub(super) fn next(&mut self) -> Result<u16, Error> {
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

    pub(super) fn at_end(&self) -> bool {
        self.left == 0 && self.reader.at_end()
    }
}

pub(super) struct CsvLine<'a> {
    pub(super) key: String,
    pub(super) param: Param,
    pub(super) feature: &'a str,
}

pub(super) fn split_key(line: &str) -> Option<(String, &str)> {
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

pub(super) fn decode_unknown<'a>(
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
                left_id: u16_at(fixed, 0),
                right_id: u16_at(fixed, 2),
                pos_id: pos_ids.next()?,
                word_cost: i16_at(fixed, 4),
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

fn append_token_line<'a>(
    line: &'a str,
    pos_ids: &mut PosIds,
    model: &mut Model,
    interner: &mut Interner<'a>,
) -> Result<(), Error> {
    let CsvLine {
        key,
        mut param,
        feature,
    } = parse_line(line).ok_or(Error::Corrupt("the lexicon text is not valid"))?;
    param.pos_id = pos_ids.next()?;
    let (param, feature) = interner.intern(model, param, feature);
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
    Ok(())
}

pub(super) fn model_from_csv(csv: &[u8], unknown: &[u8], pos: &[u8]) -> Result<Model, Error> {
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
        append_token_line(line, &mut pos_ids, &mut model, &mut interner)?;
    }
    decode_unknown(unknown, &mut pos_ids, &mut model, &mut interner)?;
    if !pos_ids.at_end() {
        return Err(Error::Corrupt("the part-of-speech ids have trailing bytes"));
    }
    Ok(model)
}

pub(super) fn restore(lexicon: &[u8], unknown: &[u8], pos: &[u8]) -> Result<Model, Error> {
    model_from_csv(&decode_lexicon(lexicon)?, unknown, pos)
}
