
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};

use super::features;
use super::trie::{self, Entry};
use super::{
    BLOCK_CODES, BLOCK_COUNT, CELLS, CHAR_BLOCKS, CHAR_INDEX, CHAR_TABLE, DICRC, FEATURES,
    FLAG_HOT_MATRIX, FLAG_INT_TEMPLATES, FLAG_WIDE_PARAMS, HEADER_BYTES, MAGIC, MATRIX, PARAMS,
    SECTIONS, TEMPLATES, TOKENS, UNKNOWN_STARTS, UNKNOWN_TOKENS, VERSION,
};
use crate::Error;
use crate::int_runtime::{is_runtime_form, runtime_form_of};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Param {
    pub(super) left_id: u16,
    pub(super) right_id: u16,
    pub(super) pos_id: u16,
    pub(super) word_cost: i16,
}

#[derive(Default, PartialEq, Debug)]
pub(super) struct Model {
    pub(super) keys: Vec<String>,
    pub(super) counts: Vec<u8>,
    pub(super) token_params: Vec<u32>,
    pub(super) token_features: Vec<u32>,
    pub(super) params: Vec<Param>,
    pub(super) features: Vec<u8>,
    pub(super) unknown_starts: Vec<u32>,
    pub(super) unknown_params: Vec<u32>,
    pub(super) unknown_features: Vec<u32>,
    pub(super) matrix: Vec<u8>,
    pub(super) templates: Vec<u8>,
    pub(super) integer_templates: bool,
    pub(super) char_table: Vec<u8>,
    pub(super) bos_feature: String,
    pub(super) unknown_feature: Option<String>,
}

pub(super) struct Runtime<'a> {
    header: Vec<u8>,
    sections: Vec<Cow<'a, [u8]>>,
}

impl Runtime<'_> {
    pub(super) fn write_to(&self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(&self.header)?;
        for section in &self.sections {
            output.write_all(section)?;
            let padding = section.len().next_multiple_of(8) - section.len();
            output.write_all(&[0; 8][..padding])?;
        }
        Ok(())
    }
}

fn le_bytes<T: Copy, const N: usize>(values: &[T], bytes: fn(T) -> [u8; N]) -> Vec<u8> {
    values.iter().flat_map(|&value| bytes(value)).collect()
}

impl Model {
    fn code_table(&self) -> Result<Vec<u16>, Error> {
        let mut frequency: HashMap<char, u32> = HashMap::new();
        for key in &self.keys {
            for char in key.chars() {
                *frequency.entry(char).or_insert(0) += 1;
            }
        }
        let mut chars: Vec<(char, u32)> = frequency.into_iter().collect();
        chars.sort_unstable_by_key(|&(char, count)| (std::cmp::Reverse(count), char));
        if chars.len() >= usize::from(u16::MAX) {
            return Err(Error::Unsupported(
                "more than 65534 different key characters",
            ));
        }
        let mut table = vec![0u16; 0x11_0000];
        for (number, &(char, _)) in chars.iter().enumerate() {
            table[char as usize] = number as u16 + 1;
        }
        Ok(table)
    }

    pub(super) fn encode_runtime(&self) -> Result<Runtime<'_>, Error> {
        let table = self.code_table()?;
        let code_count = table.iter().max().map_or(1, |&max| u32::from(max) + 1);
        let mut sequences: Vec<(Vec<u16>, u32, u8)> = Vec::with_capacity(self.keys.len());
        let mut first_token = 0u32;
        for (key, &count) in self.keys.iter().zip(&self.counts) {
            let codes = key.chars().map(|char| table[char as usize]).collect();
            sequences.push((codes, first_token, count));
            first_token += u32::from(count);
        }
        if first_token as usize != self.token_params.len() {
            return Err(Error::Unsupported(
                "token counts that do not match the tokens",
            ));
        }
        sequences.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let entries: Vec<Entry> = sequences
            .iter()
            .map(|(codes, first_token, count)| Entry {
                codes,
                first_token: *first_token,
                count: *count,
            })
            .collect();
        let cells = trie::build(&entries)?;

        let wide = self.params.len() > 1 << 16;
        let mut tokens: Vec<(u32, &str)> =
            Vec::with_capacity(self.token_features.len() + self.unknown_features.len());
        let mut next = self.token_features.iter();
        for (key, &count) in self.keys.iter().zip(&self.counts) {
            for &feature in next.by_ref().take(usize::from(count)) {
                tokens.push((feature, key));
            }
        }
        tokens.extend(self.unknown_features.iter().map(|&feature| (feature, "")));
        let encoded = features::encode::encode(&self.features, &tokens)?;
        let (system_starts, unknown_starts) = encoded.starts.split_at(self.token_features.len());
        let mut sections: [Cow<[u8]>; SECTIONS] = Default::default();
        let (index, blocks) = char_map(&table);
        sections[CHAR_INDEX] = le_bytes(&index, u16::to_le_bytes).into();
        sections[CHAR_BLOCKS] = le_bytes(&blocks, u16::to_le_bytes).into();
        sections[CELLS] = cells
            .iter()
            .flat_map(|unit| unit.iter().flat_map(|value| value.to_le_bytes()))
            .collect::<Vec<u8>>()
            .into();
        sections[TOKENS] = encode_tokens(&self.token_params, system_starts, wide).into();
        sections[PARAMS] = self
            .params
            .iter()
            .flat_map(|param| {
                let [a, b] = param.left_id.to_le_bytes();
                let [c, d] = param.right_id.to_le_bytes();
                let [e, f] = param.pos_id.to_le_bytes();
                let [g, h] = param.word_cost.to_le_bytes();
                [a, b, c, d, e, f, g, h]
            })
            .collect::<Vec<u8>>()
            .into();
        sections[FEATURES] = encoded.bytes.into();
        sections[UNKNOWN_STARTS] = le_bytes(&self.unknown_starts, u32::to_le_bytes).into();
        sections[UNKNOWN_TOKENS] = encode_tokens(&self.unknown_params, unknown_starts, wide).into();
        sections[MATRIX] = Cow::Borrowed(&self.matrix);
        sections[TEMPLATES] = if self.integer_templates && !is_runtime_form(&self.templates) {
            let hot =
                |at: usize| usize::from(u16::from_le_bytes([self.matrix[at], self.matrix[at + 1]]));
            match runtime_form_of(&self.templates, hot(4), hot(6)) {
                Ok(bytes) => bytes.into(),
                Err(Error::Unsupported(_)) => Cow::Borrowed(&self.templates),
                Err(error) => return Err(error),
            }
        } else {
            Cow::Borrowed(&self.templates)
        };
        sections[CHAR_TABLE] = Cow::Borrowed(&self.char_table);
        let unknown = self.unknown_feature.as_deref().unwrap_or_default();
        let mut dicrc = Vec::new();
        dicrc.extend_from_slice(&(self.bos_feature.len() as u32).to_le_bytes());
        dicrc.extend_from_slice(&(unknown.len() as u32).to_le_bytes());
        dicrc.extend_from_slice(self.bos_feature.as_bytes());
        dicrc.extend_from_slice(unknown.as_bytes());
        sections[DICRC] = dicrc.into();

        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        let flags = (u32::from(wide) * FLAG_WIDE_PARAMS)
            | (u32::from(!self.templates.is_empty()) * FLAG_HOT_MATRIX)
            | (u32::from(self.integer_templates) * FLAG_INT_TEMPLATES);
        out.extend_from_slice(&flags.to_le_bytes());
        for count in [
            code_count,
            self.token_params.len() as u32,
            self.params.len() as u32,
            self.unknown_starts.len().saturating_sub(1) as u32,
            0,
            0,
        ] {
            out.extend_from_slice(&count.to_le_bytes());
        }
        let mut offset = HEADER_BYTES;
        for section in &sections {
            out.extend_from_slice(&(offset as u64).to_le_bytes());
            out.extend_from_slice(&(section.len() as u64).to_le_bytes());
            offset += section.len().next_multiple_of(8);
        }
        debug_assert_eq!(out.len(), HEADER_BYTES);
        Ok(Runtime {
            header: out,
            sections: sections.into(),
        })
    }
}

fn encode_tokens(params: &[u32], starts: &[u32], wide: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(params.len() * if wide { 8 } else { 6 });
    for (&param, &start) in params.iter().zip(starts) {
        if wide {
            out.extend_from_slice(&param.to_le_bytes());
        } else {
            out.extend_from_slice(&(param as u16).to_le_bytes());
        }
        out.extend_from_slice(&start.to_le_bytes());
    }
    out
}

fn char_map(table: &[u16]) -> (Vec<u16>, Vec<u16>) {
    let mut blocks: Vec<u16> = vec![0; BLOCK_CODES];
    let zero = [0u16; BLOCK_CODES];
    let mut known: HashMap<&[u16], u16> = HashMap::new();
    known.insert(&zero, 0);
    let mut index = Vec::with_capacity(BLOCK_COUNT);
    for block in table.chunks(BLOCK_CODES) {
        let next = (blocks.len() / BLOCK_CODES) as u16;
        let number = *known.entry(block).or_insert_with(|| {
            blocks.extend_from_slice(block);
            next
        });
        index.push(number);
    }
    (index, blocks)
}
