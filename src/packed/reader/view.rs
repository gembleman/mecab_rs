use std::ops::Range;

use super::cursor::{Cursor, Step};
use super::scalar::{ASCII_LIMIT, decode};
use super::token_format::TokenFormat;
use super::{Hit, PARAM_BYTES, PackedDictionary, TokenRecord};
use crate::char_property::SentenceChars;
use crate::packed::{BLOCK_CODES, PARAMS, TOKENS};

const PARAM_FIELD_BITS: u32 = u16::BITS;
const _: () = assert!(PARAM_BYTES == 4 * size_of::<u16>() && PARAM_BYTES == size_of::<u64>());

impl std::fmt::Debug for PackedView<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PackedView")
            .field("token_count", &self.token_count)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub struct PackedView<'a> {
    char_index: &'a [u16],
    char_blocks: &'a [u16],
    cells: &'a [u8],
    tokens: &'a [u8],
    token_count: usize,
    params: &'a [u8],
    format: TokenFormat,
    ascii: &'a [u16; 128],
}

impl PackedDictionary {
    #[inline]
    pub fn view(&self) -> PackedView<'_> {
        PackedView {
            char_index: self.char_index.view(&self.map),
            char_blocks: self.char_blocks.view(&self.map),
            cells: self.cells.view(&self.map),
            tokens: &self.map[self.sections[TOKENS].clone()],
            token_count: self.token_count,
            params: &self.map[self.sections[PARAMS].clone()],
            format: self.format,
            ascii: &self.ascii,
        }
    }
}

impl PackedView<'_> {
    #[inline]
    fn code_of(&self, char: char) -> u16 {
        let scalar = char as usize;
        let block = usize::from(self.char_index[scalar / BLOCK_CODES]);
        self.char_blocks[block * BLOCK_CODES + scalar % BLOCK_CODES]
    }

    #[inline]
    pub fn common_prefix_search(&self, key: &[u8], hits: &mut Vec<Hit>) {
        hits.clear();
        self.walk_bytes(key, |hit| hits.push(hit));
    }

    #[inline]
    pub(crate) fn trie_code_of(&self, scalar: u32) -> u16 {
        match u8::try_from(scalar) {
            Ok(byte) if byte < ASCII_LIMIT => self.ascii[usize::from(byte)],
            _ => {
                let scalar = scalar as usize;
                let block = usize::from(self.char_index[scalar / BLOCK_CODES]);
                self.char_blocks[block * BLOCK_CODES + scalar % BLOCK_CODES]
            }
        }
    }

    #[inline(always)]
    pub(crate) fn walk_sentence(
        &self,
        chars: &SentenceChars,
        start: usize,
        mut found: impl FnMut(Hit),
    ) {
        let mut cursor = Cursor::new(self.cells);
        let length = chars.len();
        let mut position = start;
        while position < length {
            let (code, width) = chars.trie_step(position);
            position += width;
            match cursor.step(code) {
                Step::Stop => return,
                Step::Inner => {}
                Step::Key(first_token, token_count) => found(Hit {
                    length: position - start,
                    first_token: first_token as usize,
                    token_count: token_count as usize,
                }),
            }
        }
    }

    #[inline(always)]
    pub(super) fn walk_bytes(&self, key: &[u8], mut found: impl FnMut(Hit)) {
        let mut cursor = Cursor::new(self.cells);
        let mut at = 0;
        while let Some(&first) = key.get(at) {
            let (code, width) = if first < ASCII_LIMIT {
                (self.ascii[usize::from(first)], 1)
            } else {
                let Some((char, width)) = decode(&key[at..]) else {
                    return;
                };
                (self.code_of(char), width)
            };
            at += width;
            match cursor.step(code) {
                Step::Stop => return,
                Step::Inner => {}
                Step::Key(first_token, token_count) => found(Hit {
                    length: at,
                    first_token: first_token as usize,
                    token_count: token_count as usize,
                }),
            }
        }
    }

    #[inline]
    pub fn token(&self, index: usize) -> TokenRecord {
        self.token_at(self.tokens, self.token_count, index)
    }

    #[inline]
    pub fn tokens(&self, range: Range<usize>) -> impl Iterator<Item = TokenRecord> {
        assert!(
            range.start <= range.end && range.end <= self.token_count,
            "token range out of range"
        );
        let view = *self;
        range.map(move |index| unsafe { view.token_unchecked(view.tokens, index) })
    }

    #[inline]
    pub(super) fn token_at(&self, bytes: &[u8], count: usize, index: usize) -> TokenRecord {
        assert!(index < count, "token index out of range");
        unsafe { self.token_unchecked(bytes, index) }
    }

    #[inline(always)]
    unsafe fn token_unchecked(&self, bytes: &[u8], index: usize) -> TokenRecord {
        let format = self.format;
        let bit = index * format.bits;
        unsafe {
            let word = u64::from_le(bytes.as_ptr().add(bit >> 3).cast::<u64>().read_unaligned())
                >> (bit & 7);
            let (param, feature) = format.split(word);
            let (param, feature) = (param as usize, feature as u32);
            let record = self.params.as_ptr().add(param * PARAM_BYTES);
            let fields = u64::from_le(record.cast::<u64>().read_unaligned());
            let field = |nth: u32| (fields >> (nth * PARAM_FIELD_BITS)) as u16;
            TokenRecord {
                left_id: field(0),
                right_id: field(1),
                pos_id: field(2),
                word_cost: field(3).cast_signed(),
                feature,
            }
        }
    }
}
