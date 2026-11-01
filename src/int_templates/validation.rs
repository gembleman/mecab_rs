use super::{MAX_COST_FACTOR, MAX_SHIFT};
use crate::error::Undecodable;
use crate::hot_shape::{CELL_SUM_BOUND, MAX_KEYS};
use crate::part::PartSlices;

fn max_weight(part: &PartSlices<i32>) -> u64 {
    part.weight
        .iter()
        .map(|weight| u64::from(weight.unsigned_abs()))
        .max()
        .unwrap_or(0)
}

fn is_valid_part(part: &PartSlices<i32>, left_size: usize, right_size: usize) -> bool {
    part.has_sizes(left_size, right_size) && part.is_consistent() && part.has_sorted_spans()
}

pub(super) fn validate<'a>(
    parts: impl Iterator<Item = PartSlices<'a, i32>>,
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    exception_cells: &[u32],
) -> Result<(), Undecodable> {
    let corrupt = Undecodable("the integer templates are inconsistent");
    if left_size == 0 || right_size == 0 || left_size > MAX_KEYS || right_size > MAX_KEYS {
        return Err(Undecodable("the template matrix size is out of range"));
    }
    if shift > MAX_SHIFT || cost_factor > MAX_COST_FACTOR {
        return Err(corrupt);
    }
    let mut magnitude = 0u64;
    for part in parts {
        if !is_valid_part(&part, left_size, right_size) {
            return Err(corrupt);
        }
        magnitude += max_weight(&part);
    }
    if u128::from(magnitude) * u128::from(cost_factor) >= CELL_SUM_BOUND {
        return Err(corrupt);
    }
    validate_exception_cells(exception_cells, left_size, right_size)
}

pub(super) fn validate_exception_cells(
    exception_cells: &[u32],
    left_size: usize,
    right_size: usize,
) -> Result<(), Undecodable> {
    let cells = left_size * right_size;
    let sorted = exception_cells.windows(2).all(|pair| pair[0] < pair[1]);
    let inside = exception_cells
        .last()
        .is_none_or(|&last| (last as usize) < cells);
    if sorted && inside {
        Ok(())
    } else {
        Err(Undecodable("the integer templates are inconsistent"))
    }
}

pub(super) fn validate_exception_lengths(cells: &[u32], values: &[i16]) -> Result<(), Undecodable> {
    if cells.len() == values.len() {
        Ok(())
    } else {
        Err(Undecodable("the integer templates are inconsistent"))
    }
}
