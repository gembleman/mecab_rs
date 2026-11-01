use crate::hash::FastMap;
use std::cell::RefCell;

const KEY_SHIFT: u32 = 16;

type Counts = FastMap<u32, u32>;

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
