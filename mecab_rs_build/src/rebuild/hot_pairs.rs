use mecab_rs::__private::{HotShape, u16_at, u32_at};

const BISECTION_ROUNDS: usize = 64;
const LARGEST_PENALTY: f64 = 1e15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairCount {
    pub row: u16,
    pub column: u16,
    pub count: u32,
}

pub const PAIR_COUNT_BYTES: usize = 8;

impl PairCount {
    pub fn to_bytes(self) -> [u8; PAIR_COUNT_BYTES] {
        let [a, b] = self.row.to_le_bytes();
        let [c, d] = self.column.to_le_bytes();
        let [e, f, g, h] = self.count.to_le_bytes();
        [a, b, c, d, e, f, g, h]
    }

    pub fn from_bytes(bytes: [u8; PAIR_COUNT_BYTES]) -> Self {
        Self {
            row: u16_at(&bytes, 0),
            column: u16_at(&bytes, 2),
            count: u32_at(&bytes, 4),
        }
    }
}

pub fn hot_shape_from_pair_counts(pairs: &[PairCount], budget: usize) -> HotShape {
    let mut ordered = pairs.to_vec();
    ordered.sort_unstable_by_key(|pair| (pair.row, pair.column));
    let rows = ordered.last().map_or(0, |pair| usize::from(pair.row) + 1);
    let mut prefixes: Vec<Vec<(u32, u64)>> = vec![Vec::new(); rows];
    for pair in &ordered {
        let row = &mut prefixes[usize::from(pair.row)];
        let total = row.last().map_or(0, |&(_, total)| total) + u64::from(pair.count);
        row.push((u32::from(pair.column) + 1, total));
    }
    let lengths_at = |penalty: f64| -> Vec<u32> {
        prefixes
            .iter()
            .map(|points| best_prefix(points, penalty))
            .collect()
    };
    let cells_of = |lengths: &[u32]| lengths.iter().map(|&length| length as usize).sum::<usize>();
    let (mut low, mut high) = (0.0, LARGEST_PENALTY);
    for _ in 0..BISECTION_ROUNDS {
        let middle = (low + high) / 2.0;
        if cells_of(&lengths_at(middle)) > budget {
            low = middle;
        } else {
            high = middle;
        }
    }
    HotShape::from_lengths(lengths_at(high))
}

fn best_prefix(points: &[(u32, u64)], penalty: f64) -> u32 {
    let mut best_length = 0;
    let mut best_value = 0.0;
    for &(length, total) in points {
        let value = total as f64 - penalty * f64::from(length);
        if value > best_value {
            best_value = value;
            best_length = length;
        }
    }
    best_length
}
