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
use crate::build::text::{LINE_BUFFER_BYTES, Lines, cstr, tokenize2};

mod bigram;
mod keys;

pub(crate) use bigram::{CutFeature, bigram_keys};
pub(crate) use keys::unigram_keys;

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
}
