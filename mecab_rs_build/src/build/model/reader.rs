use std::path::Path;

use crate::tool_error::{Position, build_error, io_error};
use mecab_rs::__private::dictionary_format::CHARSET_BYTES;
use mecab_rs::__private::files::FEATURE_DEF_FILE;
use mecab_rs::__private::in_range_bytes::{u32_at, u64_at};
use mecab_rs::Error;
use mecab_rs::IoAction;

use super::fingerprint::fingerprint;
use super::writer::convert;
use super::{MODEL_COUNT_BYTES, MODEL_ENTRY_BYTES, WEIGHT_BYTES};
use crate::build::Charset;
use crate::build::text::{LINE_BUFFER_BYTES, Lines, cstr, tokenize_csv, tokenize2};

const FEATURE_BUFFER_BYTES: usize = 2048;
const MAX_FEATURE_COLUMNS: usize = 64;

pub(crate) struct Templates {
    pub(crate) unigram: Vec<Vec<u8>>,
    pub(crate) bigram: Vec<Vec<u8>>,
}

impl Templates {
    pub(crate) fn open(feature_def: &Path) -> Result<Self, Error> {
        let mut lines = Lines::open(feature_def, Some(LINE_BUFFER_BYTES))?;
        let mut templates = Self {
            unigram: Vec::new(),
            bigram: Vec::new(),
        };
        while let Some((number, line)) = lines.next_line()? {
            if line[0] == 0 || line[0] == b'#' || line[0] == b' ' {
                continue;
            }
            let at = Position::new(feature_def, number);
            let found = tokenize2(line, b"\t ", 2);
            if found.len() != 2 {
                return Err(at.build("format error"));
            }
            match cstr(line, found[0]) {
                b"UNIGRAM" => templates.unigram.push(cstr(line, found[1]).to_vec()),
                b"BIGRAM" => templates.bigram.push(cstr(line, found[1]).to_vec()),
                _ => return Err(at.build("format error")),
            }
        }
        Ok(templates)
    }
}

pub(crate) struct CostModel {
    weights: Vec<f64>,
    keys: Vec<u64>,
    templates: Templates,
}

fn template_column<'a>(
    template: &[u8],
    cursor: &mut usize,
    columns: &[&'a [u8]],
    path: &Path,
) -> Result<Option<&'a [u8]>, Error> {
    *cursor += 1;
    let optional = template.get(*cursor) == Some(&b'?');
    if optional {
        *cursor += 1;
    }
    if template.get(*cursor) != Some(&b'[') {
        return Err(build_error(path, "getIndex(): unmatched '['"));
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
                if number >= columns.len() {
                    return Ok(None);
                }
                let found = columns[number];
                if optional && (found == b"*" || found.is_empty()) {
                    return Ok(None);
                }
                return Ok(Some(found));
            }
            _ => return Err(build_error(path, "unmatched '['")),
        }
        *cursor += 1;
    }
}

fn escape_value(byte: u8) -> u8 {
    match byte {
        b'a' => 0x07,
        b'b' => 0x08,
        b't' => b'\t',
        b'n' => b'\n',
        b'v' => 0x0b,
        b'f' => 0x0c,
        b'r' => b'\r',
        b's' => b' ',
        b'\\' => b'\\',
        _ => 0,
    }
}

struct CsvColumns {
    buffer: Vec<u8>,
    starts: Vec<usize>,
}

impl CsvColumns {
    fn cut(feature: &[u8]) -> Self {
        let mut buffer = feature[..feature.len().min(FEATURE_BUFFER_BYTES - 1)].to_vec();
        buffer.push(0);
        let starts = tokenize_csv(&mut buffer, MAX_FEATURE_COLUMNS);
        Self { buffer, starts }
    }

    fn columns(&self) -> Vec<&[u8]> {
        self.starts
            .iter()
            .map(|&start| cstr(&self.buffer, start))
            .collect()
    }
}

fn up_to_nul(key: &[u8]) -> &[u8] {
    &key[..key.iter().position(|&byte| byte == 0).unwrap_or(key.len())]
}

pub(crate) fn unigram_keys(
    templates: &[Vec<u8>],
    ufeature: &[u8],
    char_type: u8,
    surface: &[u8],
    feature_def: &Path,
    mut add: impl FnMut(&[u8]),
) -> Result<(), Error> {
    let cut = CsvColumns::cut(ufeature);
    let columns = cut.columns();
    let mut key = Vec::new();
    'templates: for template in templates {
        key.clear();
        let mut cursor = 0;
        while cursor < template.len() {
            match template[cursor] {
                b'\\' => {
                    cursor += 1;
                    key.push(escape_value(template.get(cursor).copied().unwrap_or(0)));
                }
                b'%' => {
                    cursor += 1;
                    match template.get(cursor).copied().unwrap_or(0) {
                        b'F' => {
                            match template_column(template, &mut cursor, &columns, feature_def)? {
                                Some(text) => key.extend_from_slice(text),
                                None => continue 'templates,
                            }
                        }
                        b't' => key.extend_from_slice(char_type.to_string().as_bytes()),
                        b'u' => key.extend_from_slice(ufeature),
                        b'w' => key.extend_from_slice(surface),
                        other => {
                            return Err(build_error(
                                feature_def,
                                format!("unknown meta char: {}", char::from(other)),
                            ));
                        }
                    }
                }
                other => key.push(other),
            }
            cursor += 1;
        }
        add(up_to_nul(&key));
    }
    Ok(())
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
    let left = left_cut.columns();
    let right = right_cut.columns();
    let mut key = Vec::new();
    'templates: for template in templates {
        key.clear();
        let mut cursor = 0;
        while cursor < template.len() {
            match template[cursor] {
                b'\\' => {
                    cursor += 1;
                    key.push(escape_value(template.get(cursor).copied().unwrap_or(0)));
                }
                b'%' => {
                    cursor += 1;
                    match template.get(cursor).copied().unwrap_or(0) {
                        b'L' => match template_column(template, &mut cursor, &left, feature_def)? {
                            Some(text) => key.extend_from_slice(text),
                            None => continue 'templates,
                        },
                        b'R' => {
                            match template_column(template, &mut cursor, &right, feature_def)? {
                                Some(text) => key.extend_from_slice(text),
                                None => continue 'templates,
                            }
                        }
                        b'l' => key.extend_from_slice(lfeature),
                        b'r' => key.extend_from_slice(rfeature),
                        other => {
                            return Err(build_error(
                                feature_def,
                                format!("unknown meta char: {}", char::from(other)),
                            ));
                        }
                    }
                }
                other => key.push(other),
            }
            cursor += 1;
        }
        add(up_to_nul(&key));
    }
    Ok(())
}

fn require_binary_charset(model: &Path, bytes: &[u8], charset: &str) -> Result<(), Error> {
    let name = cstr(
        &bytes[MODEL_COUNT_BYTES..MODEL_COUNT_BYTES + CHARSET_BYTES],
        0,
    );
    let model_charset = Charset::parse(&String::from_utf8_lossy(name))
        .map_err(|reason| build_error(model, reason))?;
    let dictionary_charset =
        Charset::parse(charset).map_err(|reason| build_error(model, reason))?;
    if model_charset != dictionary_charset {
        return Err(build_error(
            model,
            format!(
                "model charset and dictionary charset are different. model_charset={} dictionary_charset={charset}",
                String::from_utf8_lossy(name)
            ),
        ));
    }
    Ok(())
}

fn is_binary_model(bytes: &[u8]) -> bool {
    let entry_count = (bytes.len() >= MODEL_COUNT_BYTES).then(|| u32_at(bytes, 0) as usize);
    entry_count.is_some_and(|count| {
        count * MODEL_ENTRY_BYTES + MODEL_COUNT_BYTES + CHARSET_BYTES == bytes.len()
    })
}

fn read_binary_model(
    model: &Path,
    dictionary_charset: &str,
    charset: &str,
) -> Result<Vec<u8>, Error> {
    let bytes = std::fs::read(model).map_err(io_error(IoAction::Read, model))?;
    if is_binary_model(&bytes) {
        require_binary_charset(model, &bytes, charset)?;
        Ok(bytes)
    } else {
        convert(model, dictionary_charset, charset)
    }
}

fn split_weights_and_keys(bytes: &[u8]) -> (Vec<f64>, Vec<u64>) {
    let count = u32_at(bytes, 0) as usize;
    let body = &bytes[MODEL_COUNT_BYTES + CHARSET_BYTES..];
    let weights = body[..count * WEIGHT_BYTES]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| f64::from_le_bytes(*word))
        .collect();
    let keys = body[count * WEIGHT_BYTES..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64_at(word, 0))
        .collect();
    (weights, keys)
}

impl CostModel {
    pub(crate) fn open(
        model: &Path,
        dic_dir: &Path,
        dictionary_charset: &str,
        charset: &str,
    ) -> Result<Self, Error> {
        let bytes = read_binary_model(model, dictionary_charset, charset)?;
        let (weights, keys) = split_weights_and_keys(&bytes);
        let templates = Templates::open(&dic_dir.join(FEATURE_DEF_FILE))?;
        Ok(Self {
            weights,
            keys,
            templates,
        })
    }

    fn feature_index_of(&self, key: &[u8]) -> Option<usize> {
        let wanted = fingerprint(key);
        let index = self.keys.partition_point(|&candidate| candidate < wanted);
        (self.keys.get(index) == Some(&wanted)).then_some(index)
    }

    pub(crate) fn cost(
        &self,
        ufeature: &[u8],
        char_type: u8,
        feature_def: &Path,
    ) -> Result<f64, Error> {
        let mut total = 0.0;
        unigram_keys(
            &self.templates.unigram,
            ufeature,
            char_type,
            b"",
            feature_def,
            |key| {
                if let Some(id) = self.feature_index_of(key) {
                    total += self.weights[id];
                }
            },
        )?;
        Ok(total)
    }

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
