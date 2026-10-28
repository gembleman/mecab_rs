use std::collections::BTreeMap;
use std::io::{self, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

pub const HOT_HEADER_BYTES: usize = 8;
pub(crate) const MAX_KEYS: usize = 0xFFFF;
pub(crate) const CELL_SUM_BOUND: u128 = 1 << 62;
const ROWS_PER_BLOCK: usize = 64;
const ROW_START_BYTES: usize = 4;
const COST_BYTES: usize = 2;
const BISECTION_ROUNDS: usize = 64;
const LARGEST_PENALTY: f64 = 1e15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotShape {
    lengths: Vec<u32>,
}

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

    pub fn from_bytes([a, b, c, d, e, f, g, h]: [u8; PAIR_COUNT_BYTES]) -> Self {
        Self {
            row: u16::from_le_bytes([a, b]),
            column: u16::from_le_bytes([c, d]),
            count: u32::from_le_bytes([e, f, g, h]),
        }
    }
}

impl HotShape {
    pub(crate) fn write_matrix_in_order<S>(
        &self,
        left_size: usize,
        right_size: usize,
        threads: usize,
        output: &mut impl Write,
        new_scratch: impl Fn() -> S + Sync,
        render_rows: impl Fn(&mut S, Range<usize>) -> Vec<u8> + Sync,
    ) -> io::Result<()> {
        self.write_header(left_size, right_size, output)?;
        let blocks = self.rows().div_ceil(ROWS_PER_BLOCK);
        let workers = threads.max(1);
        let next_block = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::sync_channel::<(usize, Vec<u8>)>(workers * 2);
        thread::scope(|scope| {
            for _ in 0..workers {
                let sender = sender.clone();
                let next_block = &next_block;
                let new_scratch = &new_scratch;
                let render_rows = &render_rows;
                scope.spawn(move || {
                    let mut scratch = new_scratch();
                    loop {
                        let block = next_block.fetch_add(1, Ordering::Relaxed);
                        if block >= blocks {
                            break;
                        }
                        let first = block * ROWS_PER_BLOCK;
                        let last = (first + ROWS_PER_BLOCK).min(self.rows());
                        let bytes = render_rows(&mut scratch, first..last);
                        if sender.send((block, bytes)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(sender);
            let mut pending = BTreeMap::new();
            let mut next_to_write = 0;
            for (block, bytes) in receiver.iter() {
                pending.insert(block, bytes);
                while let Some(bytes) = pending.remove(&next_to_write) {
                    output.write_all(&bytes)?;
                    next_to_write += 1;
                }
            }
            if next_to_write == blocks {
                Ok(())
            } else {
                Err(io::Error::other("a template worker stopped early"))
            }
        })
    }

    pub fn rectangle(hot_left: usize, hot_right: usize) -> Self {
        Self::from_lengths(vec![hot_left as u32; hot_right])
    }

    pub fn from_lengths(mut lengths: Vec<u32>) -> Self {
        while lengths.last() == Some(&0) {
            lengths.pop();
        }
        Self { lengths }
    }

    pub fn from_pair_counts(pairs: &[PairCount], budget: usize) -> Self {
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
        let cells_of =
            |lengths: &[u32]| lengths.iter().map(|&length| length as usize).sum::<usize>();
        let (mut low, mut high) = (0.0, LARGEST_PENALTY);
        for _ in 0..BISECTION_ROUNDS {
            let middle = (low + high) / 2.0;
            if cells_of(&lengths_at(middle)) > budget {
                low = middle;
            } else {
                high = middle;
            }
        }
        Self::from_lengths(lengths_at(high))
    }

    pub fn rows(&self) -> usize {
        self.lengths.len()
    }

    pub fn row_length(&self, row: usize) -> usize {
        self.lengths.get(row).map_or(0, |&length| length as usize)
    }

    pub fn lengths(&self) -> &[u32] {
        &self.lengths
    }

    pub fn cells(&self) -> usize {
        self.lengths.iter().map(|&length| length as usize).sum()
    }

    pub fn fits(&self, left_size: usize, right_size: usize) -> bool {
        self.cells() > 0
            && self.rows() <= right_size
            && self
                .lengths
                .iter()
                .all(|&length| length as usize <= left_size)
    }

    pub fn clamped(&self, left_size: usize, right_size: usize) -> Self {
        Self::from_lengths(
            self.lengths
                .iter()
                .take(right_size)
                .map(|&length| length.min(left_size as u32))
                .collect(),
        )
    }

    pub fn inscribed_rectangle(&self) -> (usize, usize) {
        let mut shortest = u32::MAX;
        let mut best = (0, 0);
        let mut best_area = 0;
        for (row, &length) in self.lengths.iter().enumerate() {
            shortest = shortest.min(length);
            let area = shortest as usize * (row + 1);
            if area > best_area {
                best_area = area;
                best = (shortest as usize, row + 1);
            }
        }
        best
    }

    pub fn contains_rectangle(&self, hot_left: usize, hot_right: usize) -> bool {
        self.rows() >= hot_right
            && self.lengths[..hot_right]
                .iter()
                .all(|&length| length as usize >= hot_left)
    }

    pub fn contains(&self, row: usize, column: usize) -> bool {
        column < self.row_length(row)
    }

    pub fn header_bytes(rows: usize) -> usize {
        HOT_HEADER_BYTES + ROW_START_BYTES * (rows + 1)
    }

    pub fn matrix_bytes(&self) -> usize {
        Self::header_bytes(self.rows()) + COST_BYTES * self.cells()
    }

    pub fn write_header(
        &self,
        left_size: usize,
        right_size: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if !self.fits(left_size, right_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the hot cells exceed the matrix",
            ));
        }
        for size in [left_size, right_size] {
            output.write_all(&(size as u16).to_le_bytes())?;
        }
        output.write_all(&(self.rows() as u32).to_le_bytes())?;
        let mut start = 0u32;
        output.write_all(&start.to_le_bytes())?;
        for &length in &self.lengths {
            start += length;
            output.write_all(&start.to_le_bytes())?;
        }
        Ok(())
    }

    pub fn parse(matrix: &[u8]) -> Result<(usize, usize, Self), &'static str> {
        let word = |at: usize| -> Option<[u8; 4]> { matrix.get(at..at + 4)?.try_into().ok() };
        let sizes = word(0).ok_or("the file is shorter than its header")?;
        let left_size = usize::from(u16::from_le_bytes([sizes[0], sizes[1]]));
        let right_size = usize::from(u16::from_le_bytes([sizes[2], sizes[3]]));
        let rows = word(4).ok_or("the file is shorter than its header")?;
        let rows = u32::from_le_bytes(rows) as usize;
        if left_size == 0 || right_size == 0 || rows > right_size {
            return Err("the hot block lies outside of the matrix");
        }
        let table = matrix
            .get(HOT_HEADER_BYTES..Self::header_bytes(rows))
            .ok_or("the row starts are cut off")?;
        let mut lengths = Vec::with_capacity(rows);
        let mut previous = 0u32;
        for (index, start) in table.as_chunks::<ROW_START_BYTES>().0.iter().enumerate() {
            let start = u32::from_le_bytes(*start);
            if index == 0 {
                if start != 0 {
                    return Err("the row starts do not begin at zero");
                }
            } else {
                let length = start
                    .checked_sub(previous)
                    .ok_or("the row starts are not ordered")?;
                if length as usize > left_size {
                    return Err("a hot row is longer than the matrix");
                }
                lengths.push(length);
            }
            previous = start;
        }
        let shape = Self { lengths };
        if shape.cells() == 0 || shape.lengths.last() == Some(&0) {
            return Err("the hot block lies outside of the matrix");
        }
        if shape.matrix_bytes() != matrix.len() {
            return Err("the file size does not match the header");
        }
        Ok((left_size, right_size, shape))
    }
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
