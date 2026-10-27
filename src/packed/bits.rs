
pub(super) const PADDING: usize = 7;

pub(super) const MAX_RECORD_BITS: u32 = 56;

pub(super) fn width_of(max: u64) -> u32 {
    (u64::BITS - max.leading_zeros()).max(1)
}

pub(super) const fn mask_of(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1 << width) - 1
    }
}

pub(super) fn section_bytes(count: usize, bits: usize) -> u64 {
    (count as u64 * bits as u64).div_ceil(8) + PADDING as u64
}

pub(super) struct BitWriter {
    bytes: Vec<u8>,
    accumulator: u64,
    filled: u32,
}

impl BitWriter {
    pub(super) fn with_capacity(bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
            accumulator: 0,
            filled: 0,
        }
    }

    pub(super) fn push(&mut self, value: u64, width: u32) {
        debug_assert!(width <= MAX_RECORD_BITS && value <= mask_of(width));
        self.accumulator |= value << self.filled;
        self.filled += width;
        while self.filled >= 8 {
            self.bytes.push(self.accumulator as u8);
            self.accumulator >>= 8;
            self.filled -= 8;
        }
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.bytes.push(self.accumulator as u8);
        }
        self.bytes.extend_from_slice(&[0; PADDING]);
        self.bytes
    }
}
