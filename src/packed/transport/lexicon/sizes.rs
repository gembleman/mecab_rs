use super::decode::Reader;
use super::{MAX_LEXICON_TEXT_LEN, Model, NUL, Undecodable, put_varint};

const MAX_FEATURE_BYTES: u64 = MAX_LEXICON_TEXT_LEN;
const MAX_COUNT: u64 = MAX_LEXICON_TEXT_LEN / 8;
const BYTES_PER_INPUT_BYTE: u64 = 1 << 10;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct LexiconSizes {
    pub(super) feature_bytes: usize,
    pub(super) feature_count: usize,
    pub(super) param_count: usize,
    pub(super) token_count: usize,
}

impl LexiconSizes {
    #[allow(
        clippy::naive_bytecount,
        reason = "a plain count over the feature bytes; no bytecount dependency"
    )]
    pub(super) fn of(model: &Model) -> Self {
        Self {
            feature_bytes: model.features.len(),
            feature_count: model.features.iter().filter(|&&byte| byte == NUL).count(),
            param_count: model.params.len(),
            token_count: model.token_params.len(),
        }
    }

    pub(super) fn to_bytes(self) -> Vec<u8> {
        let mut out = Vec::new();
        for value in [
            self.feature_bytes,
            self.feature_count,
            self.param_count,
            self.token_count,
        ] {
            put_varint(&mut out, value as u64);
        }
        out
    }

    pub(super) fn parse(bytes: &[u8], lexicon_len: usize) -> Result<Self, Undecodable> {
        let mut reader = Reader { bytes, at: 0 };
        let reachable = (lexicon_len as u64).saturating_mul(BYTES_PER_INPUT_BYTE);
        let feature_bytes = bounded(&mut reader, MAX_FEATURE_BYTES.min(reachable))?;
        let count_limit = MAX_COUNT.min(reachable);
        let feature_count = bounded(&mut reader, count_limit.min(feature_bytes as u64))?;
        let param_count = bounded(&mut reader, count_limit)?;
        let token_count = bounded(&mut reader, count_limit)?;
        if !reader.at_end() {
            return Err(Undecodable("the lexicon sizes have trailing bytes"));
        }
        Ok(Self {
            feature_bytes,
            feature_count,
            param_count,
            token_count,
        })
    }
}

fn bounded(reader: &mut Reader<'_>, limit: u64) -> Result<usize, Undecodable> {
    let value = reader.varint()?;
    if value > limit {
        return Err(Undecodable("a lexicon size is larger than the data allows"));
    }
    Ok(value as usize)
}
