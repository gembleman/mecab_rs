const TOP: u32 = 1 << 24;
const UINT_FLAG_LIMIT: u32 = 1024;
const UINT_MAX_BITS: usize = 63;
const UINT_SLOTS: usize = UINT_MAX_BITS + 2;
const UNIFORM_BITS: u32 = 16;
pub(crate) const MAX_TOTAL: u32 = 1 << UNIFORM_BITS;
const STATE_BYTES: usize = 5;
const CARRY_BOUNDARY: u32 = 0xFF00_0000;
const LOW_MASK: u64 = 0x00FF_FFFF;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Encode,
    Decode,
    Count,
}

pub(crate) struct Coder<'a> {
    mode: Mode,
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
    code: u32,
    unit: u32,
    input: &'a [u8],
    pos: usize,
    pub bits: f64,
    track: bool,
}

impl<'a> Coder<'a> {
    fn new(mode: Mode) -> Self {
        Coder {
            mode,
            low: 0,
            range: u32::MAX,
            cache: 0,
            cache_size: 1,
            out: Vec::new(),
            code: 0,
            unit: 0,
            input: &[],
            pos: 0,
            bits: 0.0,
            track: false,
        }
    }

    pub(crate) fn encoder(track_bits: bool) -> Coder<'static> {
        let mut coder = Coder::new(Mode::Encode);
        coder.track = track_bits;
        coder
    }

    pub(crate) fn counter() -> Coder<'static> {
        let mut coder = Coder::new(Mode::Count);
        coder.track = true;
        coder
    }

    pub(crate) fn decoder(input: &'a [u8]) -> Coder<'a> {
        let mut coder = Coder::new(Mode::Decode);
        coder.input = input;
        for _ in 0..STATE_BYTES {
            coder.code = (coder.code << 8) | u32::from(coder.next_byte());
        }
        coder
    }

    #[inline]
    pub(crate) fn is_decoder(&self) -> bool {
        self.mode == Mode::Decode
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        let b = self.input.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    pub(crate) fn overrun(&self) -> bool {
        self.pos > self.input.len() + 8
    }

    fn shift_low(&mut self) {
        if (self.low as u32) < CARRY_BOUNDARY || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = (self.low >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & LOW_MASK) << 8;
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        if self.mode == Mode::Encode {
            for _ in 0..STATE_BYTES {
                self.shift_low();
            }
        }
        self.out
    }

    #[inline]
    pub(crate) fn put(&mut self, cum: u32, freq: u32, total: u32) {
        debug_assert!(freq > 0 && cum + freq <= total && total <= MAX_TOTAL);
        if self.track {
            self.bits += (f64::from(total) / f64::from(freq)).log2();
        }
        if self.mode == Mode::Encode {
            let r = self.range / total;
            self.low += u64::from(r * cum);
            self.range = r * freq;
            while self.range < TOP {
                self.range <<= 8;
                self.shift_low();
            }
        }
    }

    #[inline]
    pub(crate) fn target(&mut self, total: u32) -> u32 {
        self.unit = self.range / total;
        (self.code / self.unit).min(total - 1)
    }

    #[inline]
    pub(crate) fn consume(&mut self, cum: u32, freq: u32) {
        self.code = self.code.wrapping_sub(self.unit.wrapping_mul(cum));
        self.range = self.unit.wrapping_mul(freq);
        while self.range < TOP {
            self.code = (self.code << 8) | u32::from(self.next_byte());
            self.range <<= 8;
        }
    }

    #[inline]
    fn uniform16(&mut self, v: u32, n: u32) -> u32 {
        if n <= 1 {
            return 0;
        }
        if self.mode == Mode::Decode {
            let t = self.target(n);
            self.consume(t, 1);
            t
        } else {
            self.put(v, 1, n);
            v
        }
    }

    pub(crate) fn uniform(&mut self, v: u64, n: u64) -> u64 {
        if n <= u64::from(MAX_TOTAL) {
            return u64::from(self.uniform16(v as u32, n as u32));
        }
        let high_n = ((n - 1) >> UNIFORM_BITS) + 1;
        let high = self.uniform(v >> UNIFORM_BITS, high_n);
        let low_n = if high == high_n - 1 {
            n - (high << UNIFORM_BITS)
        } else {
            u64::from(MAX_TOTAL)
        };
        (high << UNIFORM_BITS)
            | u64::from(self.uniform16((v & (u64::from(MAX_TOTAL) - 1)) as u32, low_n as u32))
    }

    pub(crate) fn raw_bits(&mut self, v: u64, nbits: u32) -> u64 {
        let mut out = 0u64;
        let mut left = nbits;
        while left > 0 {
            let k = left.min(UNIFORM_BITS);
            left -= k;
            let part = u64::from(self.uniform16(((v >> left) & ((1 << k) - 1)) as u32, 1 << k));
            out = (out << k) | part;
        }
        out
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Flag {
    n0: u32,
    n1: u32,
}

impl Flag {
    #[inline]
    pub(crate) fn code(&mut self, coder: &mut Coder, bit: bool, limit: u32) -> bool {
        let f0 = 2 * self.n0 + 1;
        let f1 = 2 * self.n1 + 1;
        let total = f0 + f1;
        let bit = if coder.is_decoder() {
            let slot = coder.target(total);
            let is_one = slot >= f0;
            if is_one {
                coder.consume(f0, f1);
            } else {
                coder.consume(0, f0);
            }
            is_one
        } else {
            if bit {
                coder.put(f0, f1, total);
            } else {
                coder.put(0, f0, total);
            }
            bit
        };
        if bit {
            self.n1 += 1;
        } else {
            self.n0 += 1;
        }
        if self.n0 + self.n1 > limit {
            self.n0 >>= 1;
            self.n1 >>= 1;
        }
        bit
    }
}

#[derive(Clone)]
pub(crate) struct UintModel {
    len: [Flag; UINT_SLOTS],
    top: [[Flag; 2]; UINT_SLOTS],
}

impl Default for UintModel {
    fn default() -> Self {
        UintModel {
            len: [Flag::default(); UINT_SLOTS],
            top: [[Flag::default(); 2]; UINT_SLOTS],
        }
    }
}

impl UintModel {
    pub(crate) fn code(&mut self, coder: &mut Coder, v: u64) -> u64 {
        let value_plus_one = v.wrapping_add(1);
        let bit_len = if coder.is_decoder() {
            0
        } else {
            UINT_MAX_BITS - value_plus_one.leading_zeros() as usize
        };
        let mut len = 0usize;
        while len < UINT_MAX_BITS && self.len[len].code(coder, bit_len > len, UINT_FLAG_LIMIT) {
            len += 1;
        }
        if len == 0 {
            return 0;
        }
        let mut decoded = 1u64;
        let first_bit = u64::from(self.top[len][0].code(
            coder,
            (value_plus_one >> (len - 1)) & 1 == 1,
            UINT_FLAG_LIMIT,
        ));
        decoded = (decoded << 1) | first_bit;
        if len >= 2 {
            let second_bit = u64::from(self.top[len][1].code(
                coder,
                (value_plus_one >> (len - 2)) & 1 == 1,
                UINT_FLAG_LIMIT,
            ));
            decoded = (decoded << 1) | second_bit;
            if len >= 3 {
                let rest = len as u32 - 2;
                let low_bits = coder.raw_bits(value_plus_one & ((1u64 << rest) - 1), rest);
                decoded = (decoded << rest) | low_bits;
            }
        }
        decoded - 1
    }
}
