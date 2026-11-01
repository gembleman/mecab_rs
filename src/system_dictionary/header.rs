use std::ops::Range;
use std::path::Path;

use super::{
    ARRAY_BYTES_AT, DIC_VERSION, DictionaryKind, FEATURE_BYTES_AT, HEADER_SIZE, KIND_AT,
    LEFT_SIZE_AT, MAGIC_ID, RIGHT_SIZE_AT, TOKEN_BYTES_AT, VERSION_AT, charset_name,
    is_utf8_charset,
};
use crate::error::Error;
use crate::wire::u32_at;

pub(super) struct Header {
    pub(super) left_size: usize,
    pub(super) right_size: usize,
    array_end: usize,
    tokens_end: usize,
    features_end: usize,
}

impl Header {
    pub(super) fn parse(bytes: &[u8], kind: DictionaryKind, path: &Path) -> Result<Self, Error> {
        let broken = |reason| Error::format(path, reason);
        if bytes.len() < HEADER_SIZE
            || (u32_at(bytes, 0) ^ MAGIC_ID) as usize != bytes.len()
            || u32_at(bytes, VERSION_AT) != DIC_VERSION
        {
            return Err(broken("the header, magic number or version is not valid"));
        }
        if u32_at(bytes, KIND_AT) != kind as u32 {
            return Err(broken("the dictionary type is not the expected one"));
        }
        if !is_utf8_charset(charset_name(bytes)) {
            return Err(Error::Unsupported("a dictionary charset other than UTF-8"));
        }
        let ends = section_ends(bytes);
        let Some((array_end, tokens_end, features_end)) = ends.filter(|ends| ends.2 == bytes.len())
        else {
            return Err(broken("the section sizes do not add up to the file size"));
        };
        Ok(Self {
            left_size: u32_at(bytes, LEFT_SIZE_AT) as usize,
            right_size: u32_at(bytes, RIGHT_SIZE_AT) as usize,
            array_end,
            tokens_end,
            features_end,
        })
    }

    pub(super) fn cells(&self) -> Range<usize> {
        HEADER_SIZE..self.array_end
    }

    pub(super) fn tokens(&self) -> Range<usize> {
        self.array_end..self.tokens_end
    }

    pub(super) fn features(&self) -> Range<usize> {
        self.tokens_end..self.features_end
    }
}

fn section_ends(bytes: &[u8]) -> Option<(usize, usize, usize)> {
    let end_after =
        |start: usize, size_at: usize| start.checked_add(u32_at(bytes, size_at) as usize);
    let array_end = end_after(HEADER_SIZE, ARRAY_BYTES_AT)?;
    let tokens_end = end_after(array_end, TOKEN_BYTES_AT)?;
    let features_end = end_after(tokens_end, FEATURE_BYTES_AT)?;
    Some((array_end, tokens_end, features_end))
}
