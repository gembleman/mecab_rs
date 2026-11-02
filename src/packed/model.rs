use std::borrow::Cow;
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
use crate::hash::FastMap;
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
    token_count: u32,
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

struct HeaderCounts {
    token_count: u32,
    code_count: u32,
    param_bits: u32,
    feature_bits: u32,
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

fn code_table_of(keys: &[String]) -> Result<Vec<u16>, Error> {
    let mut frequency = vec![0u32; CHAR_SPACE];
    for key in keys {
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

#[derive(PartialEq, Debug)]
pub(super) struct CodeMap {
    index: Vec<u8>,
    blocks: Vec<u8>,
    code_count: u32,
}

impl CodeMap {
    fn of(table: &[u16]) -> Self {
        let (index, blocks) = char_map(table);
        Self {
            index: le_bytes(&index, u16::to_le_bytes),
            blocks: le_bytes(&blocks, u16::to_le_bytes),
            code_count: table.iter().max().map_or(1, |&max| u32::from(max) + 1),
        }
    }
}

struct FlatCodes {
    codes: Vec<u16>,
    ends: Vec<u32>,
}

impl FlatCodes {
    fn of(keys: &[String], table: &[u16]) -> Self {
        let total: usize = keys.iter().map(|key| key.chars().count()).sum();
        let mut codes = Vec::with_capacity(total);
        let mut ends = Vec::with_capacity(keys.len());
        for key in keys {
            codes.extend(key.chars().map(|char| table[char as usize]));
            ends.push(codes.len() as u32);
        }
        Self { codes, ends }
    }

    fn of_key(&self, key: u32) -> &[u16] {
        let key = key as usize;
        let start = if key == 0 {
            0
        } else {
            self.ends[key - 1] as usize
        };
        &self.codes[start..self.ends[key] as usize]
    }
}

fn place_keys(keys: &[String], counts: &[u8], flat: FlatCodes) -> Result<trie::Placed, Error> {
    let linked = {
        let mut order: Vec<u32> = (0..keys.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| flat.of_key(a).cmp(flat.of_key(b)));
        let entries = order.iter().map(|&key| Entry {
            codes: flat.of_key(key),
            key,
            count: counts[key as usize],
        });
        trie::link(entries)?
    };
    drop(flat);
    linked.place()
}

#[cfg(feature = "xz")]
pub(crate) struct LexiconKeys {
    pub(crate) keys: Vec<String>,
    pub(crate) counts: Vec<u8>,
}

fn placed_with_code_map(keys: &[String], counts: &[u8]) -> Result<PlacedMap, Error> {
    let table = code_table_of(keys)?;
    let code_map = CodeMap::of(&table);
    let flat = FlatCodes::of(keys, &table);
    drop(table);
    let placed = place_keys(keys, counts, flat)?;
    Ok((placed, code_map))
}

#[cfg(feature = "xz")]
pub(super) struct PreparedTrie {
    fingerprint: u64,
    code_map: CodeMap,
    placed: trie::Placed,
}

#[cfg(feature = "xz")]
pub(super) type PreparedSlot = Option<Result<PreparedTrie, Error>>;

#[cfg(not(feature = "xz"))]
pub(super) type PreparedSlot = Option<Result<std::convert::Infallible, Error>>;

#[cfg(feature = "xz")]
fn keys_fingerprint(keys: &[String], counts: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = crate::hash::FastHasher::default();
    hasher.write_usize(keys.len());
    hasher.write_usize(counts.len());
    for (key, &count) in keys.iter().zip(counts) {
        hasher.write(key.as_bytes());
        hasher.write_u8(count);
    }
    hasher.finish()
}

#[cfg(feature = "xz")]
impl PreparedTrie {
    pub(super) fn build(source: &LexiconKeys) -> Result<Self, Error> {
        let fingerprint = keys_fingerprint(&source.keys, &source.counts);
        let (placed, code_map) = placed_with_code_map(&source.keys, &source.counts)?;
        Ok(Self {
            fingerprint,
            code_map,
            placed,
        })
    }

    fn describes(&self, model: &Model) -> bool {
        self.fingerprint == keys_fingerprint(&model.keys, &model.counts)
    }
}

type PlacedMap = (trie::Placed, CodeMap);

impl Model {
    fn code_table(&self) -> Result<Vec<u16>, Error> {
        code_table_of(&self.keys)
    }

    #[cfg(feature = "xz")]
    fn matching_prepared(model: &Model, prepared: PreparedSlot) -> Option<PlacedMap> {
        match prepared {
            Some(Ok(prepared)) if prepared.describes(model) => {
                Some((prepared.placed, prepared.code_map))
            }
            _ => None,
        }
    }

    #[cfg(not(feature = "xz"))]
    fn matching_prepared(_model: &Model, _prepared: PreparedSlot) -> Option<PlacedMap> {
        None
    }

    fn tabled_trie(
        &self,
        prepared: PreparedSlot,
        first_tokens: Vec<u32>,
    ) -> Result<(Vec<u8>, CodeMap), Error> {
        let (placed, code_map) = match Self::matching_prepared(self, prepared) {
            Some(found) => found,
            None => placed_with_code_map(&self.keys, &self.counts)?,
        };
        let cells = placed.finish(&first_tokens)?;
        drop(first_tokens);
        Ok((encode_cells(&cells), code_map))
    }

    fn encode_features(
        &self,
        token_features: Vec<u32>,
        token_keys: Vec<u32>,
    ) -> Result<features::encode::Encoded, Error> {
        let tokens = features::encode::Tokens::new(
            &token_features,
            &token_keys,
            &self.keys,
            &self.unknown_features,
        );
        let encoded = features::encode::encode(&self.features, tokens);
        drop(token_features);
        drop(token_keys);
        encoded
    }

    pub fn encode_runtime(&self) -> Result<Runtime<'_>, Error> {
        self.finish_runtime(self.encode_lexicon_sections(None)?)
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
        token_params: &[u32],
        unknown_params: &[u32],
        system_starts: &[u32],
        unknown_starts: &[u32],
    ) -> Result<(u32, u32), Error> {
        let param_bits = width_of(
            (token_params.iter().chain(unknown_params))
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

    fn checked_token_counts(&self) -> Result<(), Error> {
        if let Err(error) = self.check_token_counts() {
            self.code_table()?;
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn encode_lexicon_sections(
        &self,
        prepared: PreparedSlot,
    ) -> Result<LexiconSections, Error> {
        self.checked_token_counts()?;
        let layout = order::by_word_cost(self);
        self.encode_with_layout(layout, prepared)
    }

    #[cfg(feature = "xz")]
    pub(super) fn encode_lexicon_sections_releasing(
        &mut self,
        prepared: PreparedSlot,
    ) -> Result<LexiconSections, Error> {
        self.checked_token_counts()?;
        let layout = order::by_word_cost(self);
        self.token_params = Vec::new();
        self.token_features = Vec::new();
        let sections = self.encode_with_layout(layout, prepared);
        self.keys = Vec::new();
        self.counts = Vec::new();
        sections
    }

    fn encode_with_layout(
        &self,
        layout: order::Layout,
        prepared: PreparedSlot,
    ) -> Result<LexiconSections, Error> {
        let order::Layout {
            first_tokens,
            token_params,
            token_features,
            token_keys,
            params,
            unknown_params,
        } = layout;
        let (tabled, encoded) = std::thread::scope(|scope| {
            let encoded = scope.spawn(move || self.encode_features(token_features, token_keys));
            let tabled = self.tabled_trie(prepared, first_tokens);
            (
                tabled,
                encoded.join().expect("the feature encoder panicked"),
            )
        });
        let (cells, code_map) = tabled?;
        let encoded = encoded?;
        let CodeMap {
            index,
            blocks,
            code_count,
        } = code_map;
        let (system_starts, unknown_starts) = encoded.starts.split_at(token_params.len());
        let mut sections: [Cow<'static, [u8]>; SECTIONS] = Default::default();
        sections[CHAR_INDEX] = index.into();
        sections[CHAR_BLOCKS] = blocks.into();
        sections[CELLS] = cells.into();
        let (param_bits, feature_bits) = Self::token_widths(
            &token_params,
            &unknown_params,
            system_starts,
            unknown_starts,
        )?;
        sections[TOKENS] =
            encode_tokens(&token_params, system_starts, param_bits, feature_bits).into();
        sections[PARAMS] = encode_params(&params).into();
        sections[FEATURES] = encoded.bytes.into();
        sections[UNKNOWN_STARTS] = le_bytes(&self.unknown_starts, u32::to_le_bytes).into();
        sections[UNKNOWN_TOKENS] =
            encode_tokens(&unknown_params, unknown_starts, param_bits, feature_bits).into();
        Ok(LexiconSections {
            sections,
            token_count: token_params.len() as u32,
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

    fn runtime_header(&self, widths: &HeaderCounts, sections: &[Cow<'_, [u8]>]) -> Vec<u8> {
        let &HeaderCounts {
            token_count,
            code_count,
            param_bits,
            feature_bits,
        } = widths;
        let mut out = vec![0u8; HEADER_BYTES];
        out[..MAGIC_BYTES].copy_from_slice(MAGIC);
        let flags = u32::from(!self.templates.is_empty()) * (FLAG_HOT_MATRIX | FLAG_INT_TEMPLATES);
        for (at, value) in [
            (VERSION_AT, VERSION),
            (FLAGS_AT, flags),
            (CODE_COUNT_AT, code_count),
            (TOKEN_COUNT_AT, token_count),
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
            token_count,
            code_count,
            param_bits,
            feature_bits,
        } = lexicon;
        let mut sections: [Cow<'_, [u8]>; SECTIONS] = sections;
        sections[MATRIX] = Cow::Borrowed(&self.matrix);
        sections[TEMPLATES] = self.templates_section()?;
        sections[CHAR_TABLE] = Cow::Borrowed(&self.char_table);
        sections[DICRC] = self.dicrc_section().into();
        let counts = HeaderCounts {
            token_count,
            code_count,
            param_bits,
            feature_bits,
        };
        let header = self.runtime_header(&counts, &sections);
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
    let mut known: FastMap<&[u16], u16> = FastMap::default();
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
