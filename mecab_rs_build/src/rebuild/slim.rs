use std::collections::HashMap;
use std::path::Path;

use mecab_rs::__private::dictionary_format::{HEADER_SIZE, MAGIC_ID, TOKEN_SIZE};
use mecab_rs::__private::in_range_bytes::u32_at;
use mecab_rs::Error;

use super::{UNIDIC_FEATURE_COUNT, header_word};

const TOKEN_FEATURE_OFFSET: usize = 8;
const HEADER_FEATURE_SIZE_AT: usize = 32;
const HEADER_MAGIC_AT: usize = 0;
const WORD_BYTES: usize = 4;
const DOUBLE_ARRAY_SIZE_WORD: usize = 6;
const TOKEN_SIZE_WORD: usize = 7;
const FEATURE_SIZE_WORD: usize = 8;

fn split_fields(text: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, &byte) in text.iter().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                spans.push((start, index));
                start = index + 1;
            }
            _ => {}
        }
    }
    spans.push((start, text.len()));
    spans
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    Fields(Vec<usize>),
}

impl Keep {
    pub fn parse_fields(spec: &str) -> Result<Self, String> {
        let mut fields = Vec::new();
        for part in spec.split(',') {
            let number = |text: &str| {
                text.trim().parse::<usize>().map_err(|_| {
                    format!("`{part}` in the field set `{spec}` is not a number or range")
                })
            };
            match part.split_once('-') {
                Some((low, high)) => fields.extend(number(low)?..=number(high)?),
                None => fields.push(number(part)?),
            }
        }
        fields.sort_unstable();
        fields.dedup();
        Ok(Self::Fields(fields))
    }

    pub fn apply(&self, raw: &[u8]) -> Vec<u8> {
        let spans = split_fields(raw);
        match self {
            Self::Fields(kept) => {
                if spans.len() != UNIDIC_FEATURE_COUNT {
                    return raw.to_vec();
                }
                let mut out = Vec::with_capacity(raw.len());
                for (index, &(from, to)) in spans.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    if kept.binary_search(&index).is_ok() {
                        out.extend_from_slice(&raw[from..to]);
                    } else {
                        out.push(b'*');
                    }
                }
                out
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SlimReport {
    pub old_feature_size: usize,
    pub new_feature_size: usize,
    pub unique_features: usize,
    pub old_file_size: usize,
    pub new_file_size: usize,
}

struct Sections {
    token_start: usize,
    token_size: usize,
    feature_start: usize,
    feature_size: usize,
}

fn read_sections(data: &[u8], path: &Path) -> Result<Sections, Error> {
    if data.len() < HEADER_SIZE {
        return Err(Error::rebuild(path, "shorter than the 72-byte header"));
    }
    if header_word(data, HEADER_MAGIC_AT / WORD_BYTES) ^ MAGIC_ID != data.len() as u32 {
        return Err(Error::rebuild(path, "magic does not match the file size"));
    }
    let (array_size, token_size, feature_size) = (
        header_word(data, DOUBLE_ARRAY_SIZE_WORD) as usize,
        header_word(data, TOKEN_SIZE_WORD) as usize,
        header_word(data, FEATURE_SIZE_WORD) as usize,
    );
    let token_start = HEADER_SIZE + array_size;
    let feature_start = token_start + token_size;
    if token_size % TOKEN_SIZE != 0 || feature_start + feature_size != data.len() {
        return Err(Error::rebuild(
            path,
            "the sections do not add up to the file size",
        ));
    }
    Ok(Sections {
        token_start,
        token_size,
        feature_start,
        feature_size,
    })
}

#[derive(Default)]
struct SlimFeatures {
    blob: Vec<u8>,
    by_text: HashMap<Vec<u8>, u32>,
    by_offset: HashMap<u32, u32>,
}

impl SlimFeatures {
    fn place_of_text(&mut self, text: Vec<u8>) -> u32 {
        if let Some(&place) = self.by_text.get(&text) {
            return place;
        }
        let place = self.blob.len() as u32;
        self.blob.extend_from_slice(&text);
        self.blob.push(0);
        self.by_text.insert(text, place);
        place
    }

    fn place_of_offset(
        &mut self,
        source_blob: &[u8],
        offset: u32,
        keep: &Keep,
        path: &Path,
    ) -> Result<u32, Error> {
        if let Some(&place) = self.by_offset.get(&offset) {
            return Ok(place);
        }
        let tail = source_blob
            .get(offset as usize..)
            .ok_or_else(|| Error::rebuild(path, "a feature offset is past the feature section"))?;
        let length = tail
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| Error::rebuild(path, "a feature is not NUL-terminated"))?;
        let place = self.place_of_text(keep.apply(&tail[..length]));
        self.by_offset.insert(offset, place);
        Ok(place)
    }

    fn pad_to_word(&mut self) {
        while !self.blob.len().is_multiple_of(WORD_BYTES) {
            self.blob.push(0);
        }
    }
}

pub(super) fn slim_dictionary(
    data: &[u8],
    path: &Path,
    keep: &Keep,
) -> Result<(Vec<u8>, SlimReport), Error> {
    let sections = read_sections(data, path)?;
    let source_blob = &data[sections.feature_start..];
    let mut tokens = data[sections.token_start..sections.feature_start].to_vec();
    let mut features = SlimFeatures::default();
    for token in tokens.as_chunks_mut::<TOKEN_SIZE>().0.iter_mut() {
        let offset = u32_at(token, TOKEN_FEATURE_OFFSET);
        let place = features.place_of_offset(source_blob, offset, keep, path)?;
        token[TOKEN_FEATURE_OFFSET..TOKEN_FEATURE_OFFSET + WORD_BYTES]
            .copy_from_slice(&place.to_le_bytes());
    }
    let unique_features = features.by_text.len();
    features.pad_to_word();
    let total = sections.token_start + sections.token_size + features.blob.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&data[..sections.token_start]);
    out.extend_from_slice(&tokens);
    out.extend_from_slice(&features.blob);
    out[HEADER_MAGIC_AT..HEADER_MAGIC_AT + WORD_BYTES]
        .copy_from_slice(&(total as u32 ^ MAGIC_ID).to_le_bytes());
    out[HEADER_FEATURE_SIZE_AT..HEADER_FEATURE_SIZE_AT + WORD_BYTES]
        .copy_from_slice(&(features.blob.len() as u32).to_le_bytes());
    let report = SlimReport {
        old_feature_size: sections.feature_size,
        new_feature_size: features.blob.len(),
        unique_features,
        old_file_size: data.len(),
        new_file_size: total,
    };
    Ok((out, report))
}
