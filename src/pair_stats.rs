use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

const MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
const KEY_SHIFT: u32 = 16;

#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(MULTIPLIER);
        }
    }

    fn write_u32(&mut self, value: u32) {
        self.0 = u64::from(value).wrapping_mul(MULTIPLIER);
    }
}

type Counts = HashMap<u32, u32, BuildHasherDefault<KeyHasher>>;

thread_local! {
    static COUNTS: RefCell<Counts> = RefCell::new(Counts::default());
}

pub(crate) fn record(row: u16, column: u16) {
    let key = (u32::from(row) << KEY_SHIFT) | u32::from(column);
    COUNTS.with_borrow_mut(|counts| *counts.entry(key).or_insert(0) += 1);
}

pub fn take() -> Vec<(u16, u16, u32)> {
    let counts = COUNTS.with_borrow_mut(std::mem::take);
    let mut pairs: Vec<(u16, u16, u32)> = counts
        .into_iter()
        .map(|(key, count)| ((key >> KEY_SHIFT) as u16, key as u16, count))
        .collect();
    pairs.sort_unstable_by_key(|&(row, column, _)| (row, column));
    pairs
}
