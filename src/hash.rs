use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const FOLD_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
const FAST_SEED: u64 = 0x243F_6A88_85A3_08D3;
const TAIL_LENGTH_SHIFT: u32 = 59;

#[cfg(feature = "xz")]
#[derive(Default)]
pub struct IdHasher(u64);

#[cfg(feature = "xz")]
impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("IdHasher takes only pre-mixed u64 keys");
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

#[derive(Debug)]
pub struct FastHasher(u64);

impl Default for FastHasher {
    fn default() -> Self {
        Self(FAST_SEED)
    }
}

#[inline]
fn fold_multiply(left: u64, right: u64) -> u64 {
    let product = u128::from(left) * u128::from(right);
    (product as u64) ^ ((product >> u64::BITS) as u64)
}

impl FastHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = fold_multiply(self.0 ^ word, FOLD_MULTIPLIER);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for &chunk in chunks {
            self.add(u64::from_le_bytes(chunk));
        }
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last) ^ ((rest.len() as u64) << TAIL_LENGTH_SHIFT));
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u16(&mut self, value: u16) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }
}

#[cfg(feature = "xz")]
pub type IdMap<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;
#[cfg(feature = "xz")]
pub type IdSet<K> = HashSet<K, BuildHasherDefault<IdHasher>>;
pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;
pub type FastSet<K> = HashSet<K, BuildHasherDefault<FastHasher>>;
