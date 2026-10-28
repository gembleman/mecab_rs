use super::*;

pub(super) struct Stat<'a> {
    pub(super) present: usize,
    pub(super) values: SmallKeyMap<&'a [u8], u16>,
    pub(super) value_ids: Vec<u16>,
    pub(super) overflow: bool,
    pub(super) numeric: bool,
}

impl<'a> Stat<'a> {
    pub(super) fn new(entries: usize) -> Self {
        Self {
            present: 0,
            values: SmallKeyMap::default(),
            value_ids: vec![ABSENT; entries],
            overflow: false,
            numeric: true,
        }
    }

    pub(super) fn observe(&mut self, entry: usize, value: &'a [u8]) {
        self.present += 1;
        if self.numeric && parse_number(value).is_none() {
            self.numeric = false;
        }
        if self.overflow {
            return;
        }
        if let Some(&id) = self.values.get(value) {
            self.value_ids[entry] = id;
        } else if self.values.len() >= FOLD_MAX_VALUES {
            self.overflow = true;
            self.values = SmallKeyMap::default();
            self.value_ids = Vec::new();
        } else {
            let id = self.values.len() as u16;
            self.values.insert(value, id);
            self.value_ids[entry] = id;
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

pub(super) fn choose_split(values: &[u64], preferred_shift: Option<u32>) -> (u32, Vec<u64>) {
    let maximum = values.iter().copied().max().unwrap_or(0);
    let top = 64 - maximum.leading_zeros();
    let mut best = (f64::MAX, 0u32);
    let mut lows: SmallKeySet<u64> = SmallKeySet::default();
    for shift in 0..top.max(1) {
        let (hi_bits, count) = if shift == 0 {
            (top, 1)
        } else {
            lows.clear();
            let mask = (1u64 << shift) - 1;
            for &value in values {
                lows.insert(value & mask);
                if lows.len() > MAX_LOW_PARTS {
                    break;
                }
            }
            (top - shift, lows.len())
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
    if let Some(preferred_shift) = preferred_shift {
        let mask = (1u64 << preferred_shift) - 1;
        let mut set: SmallKeySet<u64> = SmallKeySet::default();
        set.extend(values.iter().map(|&value| value & mask));
        if set.len() <= MAX_LOW_PARTS {
            shift = preferred_shift;
        }
    }
    let mut table: Vec<u64> = if shift == 0 {
        vec![0]
    } else {
        let mask = (1u64 << shift) - 1;
        let mut set: SmallKeySet<u64> = SmallKeySet::default();
        set.extend(values.iter().map(|&value| value & mask));
        set.into_iter().collect()
    };
    table.sort_unstable();
    (shift, table)
}
