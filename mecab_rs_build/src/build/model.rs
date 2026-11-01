mod fingerprint;
mod reader;
mod writer;

pub(crate) use reader::{CostModel, CutFeature, Templates, bigram_keys, unigram_keys};
pub(crate) use writer::convert;

const MODEL_COUNT_BYTES: usize = 4;
const WEIGHT_BYTES: usize = 8;
const FINGERPRINT_BYTES: usize = 8;
const MODEL_ENTRY_BYTES: usize = WEIGHT_BYTES + FINGERPRINT_BYTES;
