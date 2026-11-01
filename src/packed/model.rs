use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

use super::bits::{BitWriter, MAX_RECORD_BITS, PADDING, width_of};
use super::features;
use super::order;
use super::trie::{self, Entry};
use super::{
    BLOCK_CODES, BLOCK_COUNT, CATEGORY_COUNT_AT, CELLS, CELLS_PADDING, CHAR_BLOCKS, CHAR_INDEX,
    CHAR_TABLE, CODE_COUNT_AT, DICRC, FEATURE_BITS_AT, FEATURES, FLAG_HOT_MATRIX,
    FLAG_INT_TEMPLATES, FLAGS_AT, HEADER_BYTES, MAGIC, MAGIC_BYTES, MATRIX, MAX_CODE_COUNT,
    PARAM_BITS_AT, PARAM_COUNT_AT, PARAMS, SECTION_ALIGNMENT, SECTION_ENTRY_BYTES,
    SECTION_LENGTH_IN_ENTRY, SECTION_OFFSET_IN_ENTRY, SECTION_TABLE_AT, SECTIONS, TEMPLATES,
    TOKEN_COUNT_AT, TOKENS, UNIT_BYTES, UNKNOWN_STARTS, UNKNOWN_TOKENS, VERSION, VERSION_AT,
};
use crate::Error;
use crate::dict_files::MATRIX_HOT_FILE;
use crate::hot_shape::HotShape;
use crate::int_runtime::{is_runtime_form, runtime_form_of};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Param {
    pub left_id: u16,
    pub right_id: u16,
    pub pos_id: u16,
    pub word_cost: i16,
}

#[derive(Default, PartialEq, Debug)]
pub struct Model {
    pub keys: Vec<String>,
    pub counts: Vec<u8>,
    pub token_params: Vec<u32>,
    pub token_features: Vec<u32>,
    pub params: Vec<Param>,
    pub features: Vec<u8>,
    pub unknown_starts: Vec<u32>,
    pub unknown_params: Vec<u32>,
    pub unknown_features: Vec<u32>,
    pub matrix: Vec<u8>,
    pub templates: Vec<u8>,
    pub char_table: Vec<u8>,
    pub bos_feature: String,
    pub unknown_feature: Option<String>,
}

pub(crate) struct LexiconSections {
    sections: [Cow<'static, [u8]>; SECTIONS],
    code_count: u32,
    param_bits: u32,
    feature_bits: u32,
}

impl std::fmt::Debug for Runtime<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("header_bytes", &self.header.len())
            .field("sections", &self.sections.len())
            .finish_non_exhaustive()
    }
}

pub struct Runtime<'a> {
    header: Vec<u8>,
    sections: Vec<Cow<'a, [u8]>>,
}

impl Runtime<'_> {
    pub fn write_to(&self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(&self.header)?;
        for section in &self.sections {
            output.write_all(section)?;
            let padding = section.len().next_multiple_of(SECTION_ALIGNMENT) - section.len();
            output.write_all(&[0; SECTION_ALIGNMENT][..padding])?;
        }
        Ok(())
    }
}

const CHAR_SPACE: usize = 0x11_0000;

fn le_bytes<T: Copy, const N: usize>(values: &[T], bytes: fn(T) -> [u8; N]) -> Vec<u8> {
    values.iter().flat_map(|&value| bytes(value)).collect()
}

impl Model {
    fn code_table(&self) -> Result<Vec<u16>, Error> {
        let mut frequency = vec![0u32; CHAR_SPACE];
        for key in &self.keys {
            for char in key.chars() {
                frequency[char as usize] += 1;
            }
        }
        let mut chars: Vec<(char, u32)> = frequency
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count != 0)
            .filter_map(|(code, &count)| char::from_u32(code as u32).map(|char| (char, count)))
            .collect();
        chars.sort_unstable_by_key(|&(char, count)| (std::cmp::Reverse(count), char));
        if chars.len() >= MAX_CODE_COUNT {
            return Err(Error::Unsupported(
                "more than 65534 different key characters",
            ));
        }
        let mut table = vec![0u16; CHAR_SPACE];
        for (number, &(char, _)) in chars.iter().enumerate() {
            table[char as usize] = number as u16 + 1;
        }
        Ok(table)
    }

    fn trie_cells(&self, table: &[u16], layout: &order::Layout) -> Result<Vec<u64>, Error> {
        let mut sequences: Vec<(Vec<u16>, u32, u8)> = Vec::with_capacity(self.keys.len());
        for ((key, &count), &first_token) in
            self.keys.iter().zip(&self.counts).zip(&layout.first_tokens)
        {
            let codes = key.chars().map(|char| table[char as usize]).collect();
            sequences.push((codes, first_token, count));
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
        trie::build(&entries)
    }

    fn encode_features(&self, layout: &order::Layout) -> Result<features::encode::Encoded, Error> {
        let mut tokens: Vec<(u32, &str)> =
            Vec::with_capacity(self.token_features.len() + self.unknown_features.len());
        for (&feature, &key) in layout.token_features.iter().zip(&layout.token_keys) {
            tokens.push((feature, &self.keys[key as usize]));
        }
        tokens.extend(self.unknown_features.iter().map(|&feature| (feature, "")));
        features::encode::encode(&self.features, &tokens)
    }

    pub fn encode_runtime(&self) -> Result<Runtime<'_>, Error> {
        self.finish_runtime(self.encode_lexicon_sections()?)
    }

    fn check_token_counts(&self) -> Result<(), Error> {
        let counted: usize = self.counts.iter().map(|&count| usize::from(count)).sum();
        if counted != self.token_params.len() || counted != self.token_features.len() {
            return Err(Error::Unsupported(
                "token counts that do not match the tokens",
            ));
        }
        Ok(())
    }

    fn token_widths(
        layout: &order::Layout,
        system_starts: &[u32],
        unknown_starts: &[u32],
    ) -> Result<(u32, u32), Error> {
        let param_bits = width_of(
            (layout.token_params.iter().chain(&layout.unknown_params))
                .copied()
                .max()
                .map_or(0, u64::from),
        );
        let feature_bits = width_of(
            (system_starts.iter().chain(unknown_starts))
                .copied()
                .max()
                .map_or(0, u64::from),
        );
        if param_bits + feature_bits > MAX_RECORD_BITS {
            return Err(Error::Unsupported(
                "a token needs more than 56 bits for its parameter set and feature",
            ));
        }
        Ok((param_bits, feature_bits))
    }

    pub(super) fn encode_lexicon_sections(&self) -> Result<LexiconSections, Error> {
        let counted = self.check_token_counts();
        if counted.is_err() {
            self.code_table()?;
            return counted.map(|()| unreachable!());
        }
        let layout = order::by_word_cost(self);
        let (tabled, encoded) = std::thread::scope(|scope| {
            let encoded = scope.spawn(|| self.encode_features(&layout));
            let tabled = self
                .code_table()
                .and_then(|table| Ok((self.trie_cells(&table, &layout)?, table)));
            (
                tabled,
                encoded.join().expect("the feature encoder panicked"),
            )
        });
        let (cells, table) = tabled?;
        let encoded = encoded?;
        let code_count = table.iter().max().map_or(1, |&max| u32::from(max) + 1);
        let (system_starts, unknown_starts) = encoded.starts.split_at(self.token_features.len());
        let mut sections: [Cow<'static, [u8]>; SECTIONS] = Default::default();
        let (index, blocks) = char_map(&table);
        sections[CHAR_INDEX] = le_bytes(&index, u16::to_le_bytes).into();
        sections[CHAR_BLOCKS] = le_bytes(&blocks, u16::to_le_bytes).into();
        sections[CELLS] = encode_cells(&cells).into();
        let (param_bits, feature_bits) =
            Self::token_widths(&layout, system_starts, unknown_starts)?;
        sections[TOKENS] = encode_tokens(
            &layout.token_params,
            system_starts,
            param_bits,
            feature_bits,
        )
        .into();
        sections[PARAMS] = encode_params(&layout.params).into();
        sections[FEATURES] = encoded.bytes.into();
        sections[UNKNOWN_STARTS] = le_bytes(&self.unknown_starts, u32::to_le_bytes).into();
        sections[UNKNOWN_TOKENS] = encode_tokens(
            &layout.unknown_params,
            unknown_starts,
            param_bits,
            feature_bits,
        )
        .into();
        Ok(LexiconSections {
            sections,
            code_count,
            param_bits,
            feature_bits,
        })
    }

    fn templates_section(&self) -> Result<Cow<'_, [u8]>, Error> {
        if self.templates.is_empty() || is_runtime_form(&self.templates) {
            return Ok(Cow::Borrowed(&self.templates));
        }
        let (_, _, shape) = HotShape::parse(&self.matrix)
            .map_err(|reason| Error::format(Path::new(MATRIX_HOT_FILE), reason))?;
        match runtime_form_of(&self.templates, &shape) {
            Ok(bytes) => Ok(bytes.into()),
            Err(Error::Unsupported(_)) => Err(Error::Unsupported(
                "the integer templates have no runtime form (they have no clusters, or an exception differs from the computed cost by more than 1)",
            )),
            Err(error) => Err(error),
        }
    }

    fn dicrc_section(&self) -> Vec<u8> {
        let unknown = self.unknown_feature.as_deref().unwrap_or_default();
        let mut dicrc = Vec::new();
        dicrc.extend_from_slice(&(self.bos_feature.len() as u32).to_le_bytes());
        dicrc.extend_from_slice(&(unknown.len() as u32).to_le_bytes());
        dicrc.extend_from_slice(self.bos_feature.as_bytes());
        dicrc.extend_from_slice(unknown.as_bytes());
        dicrc
    }

    fn runtime_header(&self, widths: (u32, u32, u32), sections: &[Cow<'_, [u8]>]) -> Vec<u8> {
        let (code_count, param_bits, feature_bits) = widths;
        let mut out = vec![0u8; HEADER_BYTES];
        out[..MAGIC_BYTES].copy_from_slice(MAGIC);
        let flags = u32::from(!self.templates.is_empty()) * (FLAG_HOT_MATRIX | FLAG_INT_TEMPLATES);
        for (at, value) in [
            (VERSION_AT, VERSION),
            (FLAGS_AT, flags),
            (CODE_COUNT_AT, code_count),
            (TOKEN_COUNT_AT, self.token_params.len() as u32),
            (PARAM_COUNT_AT, self.params.len() as u32),
            (
                CATEGORY_COUNT_AT,
                self.unknown_starts.len().saturating_sub(1) as u32,
            ),
            (PARAM_BITS_AT, param_bits),
            (FEATURE_BITS_AT, feature_bits),
        ] {
            out[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        let mut offset = HEADER_BYTES;
        for (number, section) in sections.iter().enumerate() {
            let at = SECTION_TABLE_AT + SECTION_ENTRY_BYTES * number;
            out[at + SECTION_OFFSET_IN_ENTRY..][..8]
                .copy_from_slice(&(offset as u64).to_le_bytes());
            out[at + SECTION_LENGTH_IN_ENTRY..][..8]
                .copy_from_slice(&(section.len() as u64).to_le_bytes());
            offset += section.len().next_multiple_of(SECTION_ALIGNMENT);
        }
        out
    }

    pub(super) fn finish_runtime(&self, lexicon: LexiconSections) -> Result<Runtime<'_>, Error> {
        let LexiconSections {
            sections,
            code_count,
            param_bits,
            feature_bits,
        } = lexicon;
        let mut sections: [Cow<'_, [u8]>; SECTIONS] = sections;
        sections[MATRIX] = Cow::Borrowed(&self.matrix);
        sections[TEMPLATES] = self.templates_section()?;
        sections[CHAR_TABLE] = Cow::Borrowed(&self.char_table);
        sections[DICRC] = self.dicrc_section().into();
        let header = self.runtime_header((code_count, param_bits, feature_bits), &sections);
        Ok(Runtime {
            header,
            sections: sections.into(),
        })
    }
}

fn encode_params(params: &[Param]) -> Vec<u8> {
    params
        .iter()
        .flat_map(|param| {
            let [a, b] = param.left_id.to_le_bytes();
            let [c, d] = param.right_id.to_le_bytes();
            let [e, f] = param.pos_id.to_le_bytes();
            let [g, h] = param.word_cost.to_le_bytes();
            [a, b, c, d, e, f, g, h]
        })
        .collect()
}

fn encode_cells(cells: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cells.len() * UNIT_BYTES + CELLS_PADDING);
    for unit in cells {
        out.extend_from_slice(&unit.to_le_bytes()[..UNIT_BYTES]);
    }
    out.extend_from_slice(&[0; CELLS_PADDING]);
    out
}

fn encode_tokens(params: &[u32], starts: &[u32], param_bits: u32, feature_bits: u32) -> Vec<u8> {
    let bits = (param_bits + feature_bits) as usize;
    let mut out = BitWriter::with_capacity((params.len() * bits).div_ceil(8) + PADDING);
    for (&param, &start) in params.iter().zip(starts) {
        out.push(u64::from(param), param_bits);
        out.push(u64::from(start), feature_bits);
    }
    out.finish()
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
