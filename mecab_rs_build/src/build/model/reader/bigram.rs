use std::path::Path;

use crate::build::text::cstr;
use crate::tool_error::build_error;
use mecab_rs::Error;

use super::CostModel;
use super::keys::{CsvColumns, escape_value, template_column, up_to_nul};

#[derive(Clone, Copy)]
enum FeatureSide {
    Rfeature,
    Lfeature,
}

enum BigramToken {
    Literal(Vec<u8>),
    Column {
        side: FeatureSide,
        number: usize,
        optional: bool,
    },
    Whole(FeatureSide),
    Fail(String),
}

pub(crate) struct BigramPlan {
    templates: Vec<Vec<BigramToken>>,
}

pub(crate) struct CutFeature {
    whole: Vec<u8>,
    buffer: Vec<u8>,
    columns: Vec<std::ops::Range<usize>>,
}

impl CutFeature {
    pub(crate) fn cut(feature: &[u8]) -> Self {
        let cut = CsvColumns::cut(feature);
        let columns = cut
            .starts
            .iter()
            .map(|&start| start..start + cstr(&cut.buffer, start).len())
            .collect();
        Self {
            whole: feature.to_vec(),
            buffer: cut.buffer,
            columns,
        }
    }

    fn column(&self, number: usize) -> Option<&[u8]> {
        self.columns
            .get(number)
            .map(|range| &self.buffer[range.clone()])
    }
}

fn parse_bigram_column(template: &[u8], cursor: &mut usize, side: FeatureSide) -> BigramToken {
    *cursor += 1;
    let optional = template.get(*cursor) == Some(&b'?');
    if optional {
        *cursor += 1;
    }
    if template.get(*cursor) != Some(&b'[') {
        return BigramToken::Fail("getIndex(): unmatched '['".to_owned());
    }
    *cursor += 1;
    let mut number = 0usize;
    loop {
        match template.get(*cursor).copied().unwrap_or(0) {
            digit @ b'0'..=b'9' => {
                number = number
                    .wrapping_mul(10)
                    .wrapping_add(usize::from(digit - b'0'));
            }
            b']' => {
                return BigramToken::Column {
                    side,
                    number,
                    optional,
                };
            }
            _ => return BigramToken::Fail("unmatched '['".to_owned()),
        }
        *cursor += 1;
    }
}

fn flush_literal(tokens: &mut Vec<BigramToken>, literal: &mut Vec<u8>) {
    if !literal.is_empty() {
        tokens.push(BigramToken::Literal(std::mem::take(literal)));
    }
}

fn parse_bigram_meta(template: &[u8], cursor: &mut usize) -> BigramToken {
    match template.get(*cursor).copied().unwrap_or(0) {
        b'L' => parse_bigram_column(template, cursor, FeatureSide::Rfeature),
        b'R' => parse_bigram_column(template, cursor, FeatureSide::Lfeature),
        b'l' => BigramToken::Whole(FeatureSide::Lfeature),
        b'r' => BigramToken::Whole(FeatureSide::Rfeature),
        other => BigramToken::Fail(format!("unknown meta char: {}", char::from(other))),
    }
}

fn compile_bigram_template(template: &[u8]) -> Vec<BigramToken> {
    let mut tokens = Vec::new();
    let mut literal = Vec::new();
    let mut cursor = 0;
    while cursor < template.len() {
        match template[cursor] {
            b'\\' => {
                cursor += 1;
                literal.push(escape_value(template.get(cursor).copied().unwrap_or(0)));
            }
            b'%' => {
                cursor += 1;
                let token = parse_bigram_meta(template, &mut cursor);
                flush_literal(&mut tokens, &mut literal);
                let failed = matches!(token, BigramToken::Fail(_));
                tokens.push(token);
                if failed {
                    return tokens;
                }
            }
            other => literal.push(other),
        }
        cursor += 1;
    }
    flush_literal(&mut tokens, &mut literal);
    tokens
}

fn assemble_bigram_key(
    tokens: &[BigramToken],
    rfeature: &CutFeature,
    lfeature: &CutFeature,
    key: &mut Vec<u8>,
    feature_def: &Path,
) -> Result<bool, Error> {
    let side_of = |side: FeatureSide| match side {
        FeatureSide::Rfeature => rfeature,
        FeatureSide::Lfeature => lfeature,
    };
    key.clear();
    for token in tokens {
        match token {
            BigramToken::Literal(bytes) => key.extend_from_slice(bytes),
            BigramToken::Whole(side) => key.extend_from_slice(&side_of(*side).whole),
            BigramToken::Column {
                side,
                number,
                optional,
            } => match side_of(*side).column(*number) {
                Some(text) if !(*optional && (text == b"*" || text.is_empty())) => {
                    key.extend_from_slice(text);
                }
                _ => return Ok(false),
            },
            BigramToken::Fail(reason) => return Err(build_error(feature_def, reason.as_str())),
        }
    }
    Ok(true)
}

impl CostModel {
    pub(crate) fn bigram_plan(&self) -> BigramPlan {
        BigramPlan {
            templates: self
                .templates
                .bigram
                .iter()
                .map(|template| compile_bigram_template(template))
                .collect(),
        }
    }

    pub(crate) fn connection_cost_cut(
        &self,
        plan: &BigramPlan,
        rfeature: &CutFeature,
        lfeature: &CutFeature,
        key: &mut Vec<u8>,
        feature_def: &Path,
    ) -> Result<f64, Error> {
        let mut total = 0.0;
        for tokens in &plan.templates {
            if assemble_bigram_key(tokens, rfeature, lfeature, key, feature_def)?
                && let Some(id) = self.feature_index_of(up_to_nul(key))
            {
                total += self.weights[id];
            }
        }
        Ok(total)
    }

}

pub(crate) fn bigram_keys(
    templates: &[Vec<u8>],
    rfeature: &[u8],
    lfeature: &[u8],
    feature_def: &Path,
    mut add: impl FnMut(&[u8]),
) -> Result<(), Error> {
    let left_cut = CsvColumns::cut(rfeature);
    let right_cut = CsvColumns::cut(lfeature);
    let features = BigramFeatures {
        rfeature,
        lfeature,
        left: left_cut.columns(),
        right: right_cut.columns(),
    };
    let mut key = Vec::new();
    for template in templates {
        if expand_bigram_template(template, &features, feature_def, &mut key)? {
            add(up_to_nul(&key));
        }
    }
    Ok(())
}

struct BigramFeatures<'a> {
    rfeature: &'a [u8],
    lfeature: &'a [u8],
    left: Vec<&'a [u8]>,
    right: Vec<&'a [u8]>,
}

fn expand_bigram_template(
    template: &[u8],
    features: &BigramFeatures<'_>,
    feature_def: &Path,
    key: &mut Vec<u8>,
) -> Result<bool, Error> {
    key.clear();
    let mut cursor = 0;
    while cursor < template.len() {
        if !push_bigram_byte(template, &mut cursor, features, feature_def, key)? {
            return Ok(false);
        }
        cursor += 1;
    }
    Ok(true)
}

fn push_bigram_byte(
    template: &[u8],
    cursor: &mut usize,
    features: &BigramFeatures<'_>,
    feature_def: &Path,
    key: &mut Vec<u8>,
) -> Result<bool, Error> {
    match template[*cursor] {
        b'\\' => {
            *cursor += 1;
            key.push(escape_value(template.get(*cursor).copied().unwrap_or(0)));
            Ok(true)
        }
        b'%' => {
            *cursor += 1;
            push_bigram_meta(template, cursor, features, feature_def, key)
        }
        other => {
            key.push(other);
            Ok(true)
        }
    }
}

fn push_bigram_meta(
    template: &[u8],
    cursor: &mut usize,
    features: &BigramFeatures<'_>,
    feature_def: &Path,
    key: &mut Vec<u8>,
) -> Result<bool, Error> {
    let columns = match template.get(*cursor).copied().unwrap_or(0) {
        b'L' => &features.left,
        b'R' => &features.right,
        b'l' => {
            key.extend_from_slice(features.lfeature);
            return Ok(true);
        }
        b'r' => {
            key.extend_from_slice(features.rfeature);
            return Ok(true);
        }
        other => {
            return Err(build_error(
                feature_def,
                format!("unknown meta char: {}", char::from(other)),
            ));
        }
    };
    match template_column(template, cursor, columns, feature_def)? {
        Some(text) => {
            key.extend_from_slice(text);
            Ok(true)
        }
        None => Ok(false),
    }
}
