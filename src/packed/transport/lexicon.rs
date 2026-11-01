use std::collections::HashMap;
use std::io::Write;

use super::{Part, Parts};
use crate::Error;
use crate::codec::{MAX_LEXICON_TEXT_LEN, encode_lexicon, visit_lexicon_records};
use crate::error::Undecodable;
use crate::packed::model::{Model, Param};
use crate::wire::{get_varint, put_varint};

mod decode;

use decode::restore;

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

fn unrestorable() -> Error {
    Error::Unsupported("a lexicon that the codec form does not restore exactly")
}

pub(super) fn encode(model: &Model) -> Result<Vec<Part>, Error> {
    check_encodable(model)?;
    let csv = model_to_csv(model)?;
    if csv.len() as u64 > MAX_LEXICON_TEXT_LEN {
        return Err(Error::Unsupported(
            "a lexicon text of more than 1 GiB cannot be coded",
        ));
    }
    let lexicon = encode_lexicon(&csv);
    let (unknown, pos) = (unknown_tokens(model), pos_runs(model));
    let restored = restore(&lexicon, &unknown, &pos).map_err(|_| unrestorable())?;
    if !same_lexicon(&restored, model) {
        return Err(unrestorable());
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
    restore(lexicon, unknown, pos).map_err(|Undecodable(reason)| parts.corrupt(reason))
}
