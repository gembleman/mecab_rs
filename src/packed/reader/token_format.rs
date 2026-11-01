use crate::packed::bits::mask_of;

#[derive(Clone, Copy)]
pub(super) struct TokenFormat {
    param_bits: u32,
    pub(super) bits: usize,
    param_mask: u64,
    feature_mask: u64,
}

impl TokenFormat {
    #[inline(always)]
    pub(super) fn split(self, word: u64) -> (u64, u64) {
        (
            word & self.param_mask,
            (word >> self.param_bits) & self.feature_mask,
        )
    }

    pub(super) fn new(param_bits: u32, feature_bits: u32) -> Self {
        Self {
            param_bits,
            bits: (param_bits + feature_bits) as usize,
            param_mask: mask_of(param_bits),
            feature_mask: mask_of(feature_bits),
        }
    }
}
