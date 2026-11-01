use std::io::Write;
use std::path::Path;

use crate::tool_error::rebuild_error;
use mecab_rs::__private::dictionary_format::{HEADER_SIZE, MAGIC_ID, TOKEN_SIZE};
use mecab_rs::__private::hash::FastMap;
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
    feature_start: usize,
    feature_size: usize,
}

fn read_sections(data: &[u8], path: &Path) -> Result<Sections, Error> {
    if data.len() < HEADER_SIZE {
        return Err(rebuild_error(path, "shorter than the 72-byte header"));
    }
    if header_word(data, HEADER_MAGIC_AT / WORD_BYTES) ^ MAGIC_ID != data.len() as u32 {
        return Err(rebuild_error(path, "magic does not match the file size"));
    }
    let (array_size, token_size, feature_size) = (
        header_word(data, DOUBLE_ARRAY_SIZE_WORD) as usize,
        header_word(data, TOKEN_SIZE_WORD) as usize,
        header_word(data, FEATURE_SIZE_WORD) as usize,
    );
    let token_start = HEADER_SIZE + array_size;
    let feature_start = token_start + token_size;
    if token_size % TOKEN_SIZE != 0 || feature_start + feature_size != data.len() {
        return Err(rebuild_error(
            path,
            "the sections do not add up to the file size",
        ));
    }
    Ok(Sections {
        token_start,
        feature_start,
        feature_size,
    })
}

#[derive(Default)]
struct SlimFeatures {
    blob: Vec<u8>,
    by_text: FastMap<Vec<u8>, u32>,
    by_offset: FastMap<u32, u32>,
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
            .ok_or_else(|| rebuild_error(path, "a feature offset is past the feature section"))?;
        let length = tail
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| rebuild_error(path, "a feature is not NUL-terminated"))?;
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

pub(super) struct SlimmedDictionary {
    fixed_len: usize,
    feature_blob: Vec<u8>,
}

impl SlimmedDictionary {
    pub(super) fn write_to(&self, data: &[u8], writer: &mut impl Write) -> std::io::Result<()> {
        writer.write_all(&data[..self.fixed_len])?;
        writer.write_all(&self.feature_blob)
    }
}

fn write_word(data: &mut [u8], at: usize, value: u32) {
    data[at..at + WORD_BYTES].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn slim_dictionary(
    data: &mut [u8],
    path: &Path,
    keep: &Keep,
) -> Result<(SlimmedDictionary, SlimReport), Error> {
    let sections = read_sections(data, path)?;
    let old_file_size = data.len();
    let (fixed, source_blob) = data.split_at_mut(sections.feature_start);
    let mut features = SlimFeatures::default();
    let tokens = &mut fixed[sections.token_start..];
    for token in tokens.as_chunks_mut::<TOKEN_SIZE>().0.iter_mut() {
        let offset = u32_at(token, TOKEN_FEATURE_OFFSET);
        let place = features.place_of_offset(source_blob, offset, keep, path)?;
        write_word(token, TOKEN_FEATURE_OFFSET, place);
    }
    let unique_features = features.by_text.len();
    features.pad_to_word();
    let total = sections.feature_start + features.blob.len();
    write_word(fixed, HEADER_MAGIC_AT, total as u32 ^ MAGIC_ID);
    write_word(fixed, HEADER_FEATURE_SIZE_AT, features.blob.len() as u32);
    let report = SlimReport {
        old_feature_size: sections.feature_size,
        new_feature_size: features.blob.len(),
        unique_features,
        old_file_size,
        new_file_size: total,
    };
    let slimmed = SlimmedDictionary {
        fixed_len: sections.feature_start,
        feature_blob: features.blob,
    };
    Ok((slimmed, report))
}
