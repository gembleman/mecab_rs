pub(super) const PADDING: usize = 7;

pub(super) const MAX_RECORD_BITS: u32 = 56;

const PART_BITS: u32 = 32;

pub(super) fn width_of(max: u64) -> u32 {
    max.bit_width().max(1)
}

pub(super) const fn mask_of(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1 << width) - 1
    }
}

#[inline(always)]
pub(super) fn narrow_mask(width: u32) -> u64 {
    debug_assert!(width <= MAX_RECORD_BITS);
    (1u64 << width) - 1
}

#[inline(always)]
pub(super) fn extract(word: u64, bit: usize, width: u32) -> u64 {
    (word >> (bit & 7)) & narrow_mask(width)
}

pub(super) fn section_bytes(count: usize, bits: usize) -> u64 {
    (count as u64 * bits as u64).div_ceil(8) + PADDING as u64
}

#[derive(Default)]
pub(super) struct BitWriter {
    pub(super) bytes: Vec<u8>,
    accumulator: u64,
    filled: u32,
}

impl BitWriter {
    pub(super) fn with_capacity(bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
            ..Self::default()
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

    pub(super) fn put(&mut self, mut value: u64, mut width: u32) {
        while width > 0 {
            let part = width.min(PART_BITS);
            self.push(value & mask_of(part), part);
            value = value.checked_shr(part).unwrap_or(0);
            width -= part;
        }
    }

    pub(super) fn put_bytes(&mut self, data: &[u8]) {
        self.finish_record();
        self.bytes.extend_from_slice(data);
        self.bytes.push(0);
    }

    pub(super) fn finish_record(&mut self) {
        self.flush_partial();
        self.accumulator = 0;
        self.filled = 0;
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        self.flush_partial();
        self.bytes.extend_from_slice(&[0; PADDING]);
        self.bytes
    }

    fn flush_partial(&mut self) {
        if self.filled > 0 {
            self.bytes.push(self.accumulator as u8);
        }
    }
}
