
use std::collections::HashMap;
use std::ops::Range;

use super::{Part, Parts};
use crate::Error;
use crate::packed::model::{Model, Param};

const ENCODING_PLAIN: u32 = 0;
const ENCODING_PACKED: u32 = 1;
const SURFACE_MARKER: u8 = 1;
const PREVIOUS_MARKER: u8 = 2;
const FIELD_MARKER: u8 = 3;
const MAX_MARKED_FIELD: usize = 0x1F - FIELD_MARKER as usize;
const META_COUNT: usize = 7;

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

struct Varints<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Varints<'_> {
    fn next(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

fn id_width(count: usize) -> usize {
    match count {
        0..=0x1_0000 => 2,
        0x1_0001..=0x100_0000 => 3,
        _ => 4,
    }
}

fn shrink(feature: &[u8], surface: Option<&[u8]>, previous: &[u8], out: &mut Vec<u8>) {
    let mut first: HashMap<&[u8], usize> = HashMap::new();
    let mut before = previous.split(|&byte| byte == b',');
    for (number, field) in feature.split(|&byte| byte == b',').enumerate() {
        if number > 0 {
            out.push(b',');
        }
        let same_place = before.next();
        if field == b"*" || field.is_empty() {
            out.extend_from_slice(field);
        } else if Some(field) == same_place {
            out.push(PREVIOUS_MARKER);
        } else if Some(field) == surface {
            out.push(SURFACE_MARKER);
        } else if let Some(&earlier) = first.get(field) {
            out.push(FIELD_MARKER + earlier as u8);
        } else {
            if number <= MAX_MARKED_FIELD {
                first.insert(field, number);
            }
            out.extend_from_slice(field);
        }
    }
}

fn restore(
    shrunk: &[u8],
    surface: Option<&[u8]>,
    previous: Range<usize>,
    out: &mut Vec<u8>,
) -> Option<()> {
    let mut before: Vec<Range<usize>> = Vec::new();
    let mut from = previous.start;
    for field in out[previous.clone()].split(|&byte| byte == b',') {
        if previous.is_empty() {
            break;
        }
        before.push(from..from + field.len());
        from += field.len() + 1;
    }
    let mut before = before.into_iter();
    let mut fields: Vec<(usize, usize)> = Vec::new();
    for (number, field) in shrunk.split(|&byte| byte == b',').enumerate() {
        if number > 0 {
            out.push(b',');
        }
        let same_place = before.next();
        let begin = out.len();
        match field {
            [SURFACE_MARKER] => out.extend_from_slice(surface?),
            [PREVIOUS_MARKER] => out.extend_from_within(same_place?),
            [marker @ FIELD_MARKER..=0x1F] => {
                let (from, to) = *fields.get(usize::from(marker - FIELD_MARKER))?;
                out.extend_from_within(from..to);
            }
            _ => out.extend_from_slice(field),
        }
        fields.push((begin, out.len()));
    }
    Some(())
}

fn plain(name: &'static str, bytes: Vec<u8>) -> Part {
    Part {
        name,
        encoding: ENCODING_PLAIN,
        bytes,
    }
}

fn packed(name: &'static str, bytes: Vec<u8>) -> Part {
    Part {
        name,
        encoding: ENCODING_PACKED,
        bytes,
    }
}

pub(super) fn encode(model: &Model) -> Result<Vec<Part>, Error> {
    let mut keys = Vec::new();
    let mut previous: &str = "";
    for key in &model.keys {
        if key.contains('\0') {
            return Err(Error::Unsupported("a key with a NUL byte"));
        }
        let shared = key
            .char_indices()
            .zip(previous.chars())
            .take_while(|((_, a), b)| a == b)
            .count();
        let skip = key
            .char_indices()
            .nth(shared)
            .map_or(key.len(), |(at, _)| at);
        put_varint(&mut keys, shared as u64);
        keys.extend_from_slice(&key.as_bytes()[skip..]);
        keys.push(0);
        previous = key;
    }

    let width = id_width(model.params.len());
    let param_numbers: Vec<u8> = model
        .token_params
        .iter()
        .chain(&model.unknown_params)
        .flat_map(|number| number.to_le_bytes()[..width].to_vec())
        .collect();
    let params: Vec<u8> = model
        .params
        .iter()
        .flat_map(|param| {
            let [a, b] = param.left_id.to_le_bytes();
            let [c, d] = param.right_id.to_le_bytes();
            let [e, f] = param.pos_id.to_le_bytes();
            let [g, h] = param.word_cost.to_le_bytes();
            [a, b, c, d, e, f, g, h]
        })
        .collect();

    let mut references = Vec::new();
    let mut first_key: Vec<Option<usize>> = Vec::new();
    let mut key_of_token = Vec::with_capacity(model.token_params.len());
    for (number, &count) in model.counts.iter().enumerate() {
        key_of_token.extend(std::iter::repeat_n(number, usize::from(count)));
    }
    let tokens = model.token_features.iter().chain(&model.unknown_features);
    for (token, &feature) in tokens.enumerate() {
        let seen = first_key.len() as u64;
        if u64::from(feature) == seen {
            first_key.push(key_of_token.get(token).copied());
        }
        put_varint(&mut references, seen - u64::from(feature));
    }

    let mut unknown_counts = Vec::new();
    for pair in model.unknown_starts.windows(2) {
        put_varint(&mut unknown_counts, u64::from(pair[1] - pair[0]));
    }

    let shrinkable = !model.features.iter().any(|&byte| byte != 0 && byte < 0x20);
    let mut features = Vec::with_capacity(model.features.len());
    let mut previous: &[u8] = &[];
    for (number, feature) in model.features.split(|&byte| byte == 0).enumerate() {
        if number == first_key.len() {
            break;
        }
        if shrinkable {
            let surface = first_key[number].map(|key| model.keys[key].as_bytes());
            shrink(feature, surface, previous, &mut features);
            previous = feature;
        } else {
            features.extend_from_slice(feature);
        }
        features.push(0);
    }

    let meta: Vec<u8> = [
        model.keys.len(),
        model.token_params.len(),
        model.unknown_params.len(),
        model.params.len(),
        first_key.len(),
        model.unknown_starts.len().saturating_sub(1),
        width,
    ]
    .iter()
    .flat_map(|&value| (value as u32).to_le_bytes())
    .collect();

    Ok(vec![
        plain("meta", meta),
        packed("keys", keys),
        plain("counts", model.counts.clone()),
        plain("param numbers", param_numbers),
        plain("params", params),
        packed("feature references", references),
        packed("unknown counts", unknown_counts),
        Part {
            name: "features",
            encoding: u32::from(shrinkable),
            bytes: features,
        },
    ])
}

pub(super) fn decode(parts: &Parts, model: &mut Model) -> Result<(), Error> {
    let corrupt = |reason| parts.corrupt(reason);
    let meta = parts.plain("meta", ENCODING_PLAIN)?;
    if meta.len() != META_COUNT * 4 {
        return Err(corrupt("the meta part has a wrong length"));
    }
    let meta: Vec<usize> = meta
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&word| u32::from_le_bytes(word) as usize)
        .collect();
    let [
        key_count,
        system_count,
        unknown_count,
        param_count,
        feature_count,
        category_count,
        width,
    ] = meta[..]
    else {
        unreachable!("the meta part has seven numbers");
    };
    if width != id_width(param_count) {
        return Err(corrupt("the parameter number width is not valid"));
    }
    let token_count = system_count
        .checked_add(unknown_count)
        .ok_or_else(|| corrupt("too many tokens"))?;

    let keys_part = parts.plain("keys", ENCODING_PACKED)?;
    let counts = parts.plain("counts", ENCODING_PLAIN)?.to_vec();
    let mut keys: Vec<String> = Vec::with_capacity(key_count.min(1 << 24));
    let mut varints = Varints {
        bytes: keys_part,
        at: 0,
    };
    while !varints.at_end() {
        let shared = varints.next().ok_or_else(|| corrupt("a key is cut off"))? as usize;
        let at = varints.at;
        let end = keys_part[at..]
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| corrupt("a key is cut off"))?
            + at;
        let previous = keys.last().map_or("", String::as_str);
        let mut chars = previous.char_indices().skip(shared);
        let skip = match chars.next() {
            Some((skip, _)) => skip,
            None if previous.chars().count() == shared => previous.len(),
            None => return Err(corrupt("a key shares more than the key before it")),
        };
        let mut key = previous.as_bytes()[..skip].to_vec();
        key.extend_from_slice(&keys_part[at..end]);
        keys.push(String::from_utf8(key).map_err(|_| corrupt("a key is not valid UTF-8"))?);
        varints.at = end + 1;
    }
    if keys.len() != key_count || counts.len() != key_count {
        return Err(corrupt("the number of keys does not match"));
    }
    if counts
        .iter()
        .map(|&count| usize::from(count))
        .sum::<usize>()
        != system_count
    {
        return Err(corrupt("the token counts do not match the tokens"));
    }

    let params_part = parts.plain("params", ENCODING_PLAIN)?;
    if params_part.len() != param_count * 8 {
        return Err(corrupt("the parameter sets do not match their number"));
    }
    let params: Vec<Param> = params_part
        .as_chunks::<8>()
        .0
        .iter()
        .map(|param| Param {
            left_id: u16::from_le_bytes([param[0], param[1]]),
            right_id: u16::from_le_bytes([param[2], param[3]]),
            pos_id: u16::from_le_bytes([param[4], param[5]]),
            word_cost: i16::from_le_bytes([param[6], param[7]]),
        })
        .collect();
    let numbers = parts.plain("param numbers", ENCODING_PLAIN)?;
    if numbers.len() != token_count * width {
        return Err(corrupt("the parameter numbers do not match the tokens"));
    }
    let all_params: Vec<u32> = numbers
        .chunks_exact(width)
        .map(|number| {
            let mut bytes = [0u8; 4];
            bytes[..width].copy_from_slice(number);
            u32::from_le_bytes(bytes)
        })
        .collect();
    if all_params
        .iter()
        .any(|&number| number as usize >= param_count)
    {
        return Err(corrupt("a parameter number is out of range"));
    }

    let references_part = parts.plain("feature references", ENCODING_PACKED)?;
    let mut references = Varints {
        bytes: references_part,
        at: 0,
    };
    let mut all_features = Vec::with_capacity(token_count);
    let mut key_of_feature: Vec<Option<usize>> = Vec::new();
    let mut key = 0;
    let mut left_in_key = counts.first().map_or(0, |&count| usize::from(count));
    for token in 0..token_count {
        let back = references
            .next()
            .ok_or_else(|| corrupt("the feature references end early"))?;
        let seen = key_of_feature.len() as u64;
        let feature = seen
            .checked_sub(back)
            .ok_or_else(|| corrupt("a feature reference is out of range"))?;
        if feature == seen {
            key_of_feature.push((token < system_count).then_some(key));
        }
        all_features.push(feature as u32);
        if token < system_count {
            left_in_key -= 1;
            while left_in_key == 0 && key + 1 < counts.len() {
                key += 1;
                left_in_key = usize::from(counts[key]);
            }
        }
    }
    if !references.at_end() || key_of_feature.len() != feature_count {
        return Err(corrupt("the feature references do not match the features"));
    }

    let unknown_counts_part = parts.plain("unknown counts", ENCODING_PACKED)?;
    let mut unknown_starts = vec![0u32];
    let mut counts_reader = Varints {
        bytes: unknown_counts_part,
        at: 0,
    };
    for _ in 0..category_count {
        let count = counts_reader
            .next()
            .ok_or_else(|| corrupt("the unknown counts end early"))?;
        let last = *unknown_starts.last().expect("starts has a first element");
        unknown_starts.push(
            u32::try_from(u64::from(last) + count)
                .map_err(|_| corrupt("too many unknown tokens"))?,
        );
    }
    if !counts_reader.at_end()
        || unknown_starts.last().map(|&last| last as usize) != Some(unknown_count)
    {
        return Err(corrupt("the unknown counts do not match the tokens"));
    }

    let (encoding, features_part) = parts.get("features")?;
    if encoding > 1 {
        return Err(Error::Unsupported("a part with an unknown encoding"));
    }
    let mut features = Vec::with_capacity(features_part.len() * 2);
    let mut previous = 0..0;
    let mut seen = 0;
    for shrunk in features_part.split(|&byte| byte == 0) {
        if seen == feature_count {
            if !shrunk.is_empty() {
                return Err(corrupt("the features do not end with a NUL byte"));
            }
            break;
        }
        if encoding == 1 {
            let surface = key_of_feature[seen].map(|key| keys[key].as_bytes());
            let begin = features.len();
            restore(shrunk, surface, previous.clone(), &mut features)
                .ok_or_else(|| corrupt("a feature refers to a missing field"))?;
            previous = begin..features.len();
        } else {
            features.extend_from_slice(shrunk);
        }
        features.push(0);
        seen += 1;
    }
    if seen != feature_count {
        return Err(corrupt("the number of features does not match"));
    }
    if str::from_utf8(&features).is_err() {
        return Err(corrupt("the features are not valid UTF-8"));
    }

    let (token_params, unknown_params) = all_params.split_at(system_count);
    let (token_features, unknown_features) = all_features.split_at(system_count);
    model.keys = keys;
    model.counts = counts;
    model.token_params = token_params.to_vec();
    model.token_features = token_features.to_vec();
    model.params = params;
    model.features = features;
    model.unknown_starts = unknown_starts;
    model.unknown_params = unknown_params.to_vec();
    model.unknown_features = unknown_features.to_vec();
    Ok(())
}
