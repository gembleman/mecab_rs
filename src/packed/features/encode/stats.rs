use super::{ABSENT, FOLD_MAX_VALUES, FastMap, MAX_LOW_PARTS, bits_for, parse_number};
use crate::hash::FastSet;

pub(super) struct Stat<'a> {
    pub(super) present: usize,
    pub(super) values: FastMap<&'a [u8], u16>,
    pub(super) value_ids: Vec<u16>,
    pub(super) overflow: bool,
    pub(super) numeric: bool,
    last: Option<(&'a [u8], u16)>,
}

impl<'a> Stat<'a> {
    pub(super) fn new() -> Self {
        Self {
            present: 0,
            values: FastMap::default(),
            value_ids: Vec::new(),
            overflow: false,
            numeric: true,
            last: None,
        }
    }

    fn record(&mut self, entry: usize, id: u16) {
        debug_assert!(self.value_ids.len() <= entry);
        self.value_ids.resize(entry, ABSENT);
        self.value_ids.push(id);
    }

    pub(super) fn id_of(&self, entry: usize) -> u16 {
        self.value_ids.get(entry).copied().unwrap_or(ABSENT)
    }

    pub(super) fn trim(&mut self) {
        self.value_ids.shrink_to_fit();
    }

    pub(super) fn observe(&mut self, entry: usize, value: &'a [u8]) {
        self.present += 1;
        if let Some((last, id)) = self.last
            && last == value
        {
            self.record(entry, id);
            return;
        }
        if self.numeric && parse_number(value).is_none() {
            self.numeric = false;
        }
        if self.overflow {
            return;
        }
        if let Some(&id) = self.values.get(value) {
            self.record(entry, id);
            self.last = Some((value, id));
        } else if self.values.len() >= FOLD_MAX_VALUES {
            self.overflow = true;
            self.values = FastMap::default();
            self.value_ids = Vec::new();
            self.last = None;
        } else {
            let id = self.values.len() as u16;
            self.values.insert(value, id);
            self.record(entry, id);
            self.last = Some((value, id));
        }
    }

    pub(super) fn folds(&self) -> bool {
        !self.overflow
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    Folded,
    Text,
    Number(usize),
    High(usize),
}

const BITMAP_SHIFT_MAX: u32 = 20;
const WORD_BITS: usize = 64;

fn low_mask(shift: u32) -> u64 {
    (1u64 << shift) - 1
}

fn count_lows(values: &[u64], shift: u32) -> usize {
    let mask = low_mask(shift);
    if shift <= BITMAP_SHIFT_MAX {
        let mut bits = vec![0u64; (1usize << shift).div_ceil(WORD_BITS)];
        let mut count = 0usize;
        for &value in values {
            let low = (value & mask) as usize;
            let word = &mut bits[low / WORD_BITS];
            let bit = 1u64 << (low % WORD_BITS);
            if *word & bit == 0 {
                *word |= bit;
                count += 1;
                if count > MAX_LOW_PARTS {
                    break;
                }
            }
        }
        return count;
    }
    let mut lows: FastSet<u64> = FastSet::default();
    for &value in values {
        lows.insert(value & mask);
        if lows.len() > MAX_LOW_PARTS {
            break;
        }
    }
    lows.len()
}

fn collect_lows(values: &[u64], shift: u32) -> Vec<u64> {
    let mask = low_mask(shift);
    if shift <= BITMAP_SHIFT_MAX {
        let mut bits = vec![0u64; (1usize << shift).div_ceil(WORD_BITS)];
        for &value in values {
            let low = (value & mask) as usize;
            bits[low / WORD_BITS] |= 1u64 << (low % WORD_BITS);
        }
        let mut table = Vec::new();
        for (index, &word) in bits.iter().enumerate() {
            let mut rest = word;
            while rest != 0 {
                table.push((index * WORD_BITS) as u64 + u64::from(rest.trailing_zeros()));
                rest &= rest - 1;
            }
        }
        return table;
    }
    let mut set: FastSet<u64> = FastSet::default();
    set.extend(values.iter().map(|&value| value & mask));
    let mut table: Vec<u64> = set.into_iter().collect();
    table.sort_unstable();
    table
}

pub(super) fn choose_split(values: &[u64], preferred_shift: Option<u32>) -> (u32, Vec<u64>) {
    let maximum = values.iter().copied().max().unwrap_or(0);
    let top = 64 - maximum.leading_zeros();
    let mut best = (f64::MAX, 0u32);
    for shift in 0..top.max(1) {
        let (hi_bits, count) = if shift == 0 {
            (top, 1)
        } else {
            (top - shift, count_lows(values, shift))
        };
        if count > MAX_LOW_PARTS {
            break;
        }
        let cost = f64::from(hi_bits)
            + f64::from(bits_for(count as u64))
            + 64.0 * count as f64 / values.len().max(1) as f64;
        if cost < best.0 {
            best = (cost, shift);
        }
    }
    let mut shift = best.1;
    if let Some(preferred_shift) = preferred_shift
        && count_lows(values, preferred_shift) <= MAX_LOW_PARTS
    {
        shift = preferred_shift;
    }
    let table = if shift == 0 {
        vec![0]
    } else {
        collect_lows(values, shift)
    };
    (shift, table)
}
