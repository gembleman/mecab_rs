const TOP: u32 = 1 << 24;
const UINT_FLAG_LIMIT: u32 = 1024;
const UINT_MAX_BITS: usize = 63;
const UINT_SLOTS: usize = UINT_MAX_BITS + 2;
pub const MAX_TOTAL: u32 = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Encode,
    Decode,
    Count,
}

pub struct Coder<'a> {
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

    pub fn encoder(track_bits: bool) -> Coder<'static> {
        let mut c = Coder::new(Mode::Encode);
        c.track = track_bits;
        c
    }

    pub fn counter() -> Coder<'static> {
        let mut c = Coder::new(Mode::Count);
        c.track = true;
        c
    }

    pub fn decoder(input: &'a [u8]) -> Coder<'a> {
        let mut c = Coder::new(Mode::Decode);
        c.input = input;
        for _ in 0..5 {
            c.code = (c.code << 8) | c.next_byte() as u32;
        }
        c
    }

    #[inline]
    pub fn is_decoder(&self) -> bool {
        self.mode == Mode::Decode
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        let b = self.input.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    pub fn overrun(&self) -> bool {
        self.pos > self.input.len() + 8
    }

    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
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
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.mode == Mode::Encode {
            for _ in 0..5 {
                self.shift_low();
            }
        }
        self.out
    }

    #[inline]
    pub fn put(&mut self, cum: u32, freq: u32, total: u32) {
        debug_assert!(freq > 0 && cum + freq <= total && total <= MAX_TOTAL);
        if self.track {
            self.bits += (total as f64 / freq as f64).log2();
        }
        if self.mode == Mode::Encode {
            let r = self.range / total;
            self.low += (r * cum) as u64;
            self.range = r * freq;
            while self.range < TOP {
                self.range <<= 8;
                self.shift_low();
            }
        }
    }

    #[inline]
    pub fn target(&mut self, total: u32) -> u32 {
        self.unit = self.range / total;
        (self.code / self.unit).min(total - 1)
    }

    #[inline]
    pub fn consume(&mut self, cum: u32, freq: u32) {
        self.code = self.code.wrapping_sub(self.unit.wrapping_mul(cum));
        self.range = self.unit.wrapping_mul(freq);
        while self.range < TOP {
            self.code = (self.code << 8) | self.next_byte() as u32;
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

    pub fn uniform(&mut self, v: u64, n: u64) -> u64 {
        if n <= MAX_TOTAL as u64 {
            return self.uniform16(v as u32, n as u32) as u64;
        }
        let high_n = ((n - 1) >> 16) + 1;
        let high = self.uniform(v >> 16, high_n);
        let low_n = if high == high_n - 1 {
            n - (high << 16)
        } else {
            1 << 16
        };
        (high << 16) | self.uniform16((v & 0xFFFF) as u32, low_n as u32) as u64
    }

    pub fn raw_bits(&mut self, v: u64, nbits: u32) -> u64 {
        let mut out = 0u64;
        let mut left = nbits;
        while left > 0 {
            let k = left.min(16);
            left -= k;
            let part = self.uniform16(((v >> left) & ((1 << k) - 1)) as u32, 1 << k) as u64;
            out = (out << k) | part;
        }
        out
    }
}

#[derive(Clone, Copy, Default)]
pub struct Flag {
    n0: u32,
    n1: u32,
}

impl Flag {
    #[inline]
    pub fn code(&mut self, c: &mut Coder, bit: bool, limit: u32) -> bool {
        let f0 = 2 * self.n0 + 1;
        let f1 = 2 * self.n1 + 1;
        let total = f0 + f1;
        let bit = if c.is_decoder() {
            let t = c.target(total);
            let b = t >= f0;
            if b {
                c.consume(f0, f1);
            } else {
                c.consume(0, f0);
            }
            b
        } else {
            if bit {
                c.put(f0, f1, total);
            } else {
                c.put(0, f0, total);
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
pub struct UintModel {
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
    pub fn code(&mut self, c: &mut Coder, v: u64) -> u64 {
        let w = v.wrapping_add(1);
        let n = if c.is_decoder() {
            0
        } else {
            UINT_MAX_BITS - w.leading_zeros() as usize
        };
        let mut len = 0usize;
        while len < UINT_MAX_BITS && self.len[len].code(c, n > len, UINT_FLAG_LIMIT) {
            len += 1;
        }
        if len == 0 {
            return 0;
        }
        let mut x = 1u64;
        let b1 = self.top[len][0].code(c, (w >> (len - 1)) & 1 == 1, UINT_FLAG_LIMIT) as u64;
        x = (x << 1) | b1;
        if len >= 2 {
            let b2 = self.top[len][1].code(c, (w >> (len - 2)) & 1 == 1, UINT_FLAG_LIMIT) as u64;
            x = (x << 1) | b2;
            if len >= 3 {
                let rest = len as u32 - 2;
                let r = c.raw_bits(w & ((1u64 << rest) - 1), rest);
                x = (x << rest) | r;
            }
        }
        x - 1
    }
}
