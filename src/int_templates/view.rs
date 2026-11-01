use std::fmt;
use std::io::{self, Write};

use super::{IntTemplates, int_cost};
use crate::hot_shape::HotShape;
use crate::part::{IntPart, PartSlices, RowScratch};

impl fmt::Debug for IntView<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IntView")
            .field("cost_factor", &self.cost_factor)
            .field("shift", &self.shift)
            .field("left_size", &self.left_size)
            .field("right_size", &self.right_size)
            .field("parts", &self.parts.len())
            .finish_non_exhaustive()
    }
}

pub struct IntView<'a> {
    pub(crate) cost_factor: u32,
    pub(crate) shift: u32,
    pub(crate) left_size: usize,
    pub(crate) right_size: usize,
    parts: Vec<PartSlices<'a, i32>>,
    exception_cells: &'a [u32],
    exception_values: &'a [i16],
}

impl IntView<'_> {
    pub fn render_computed_row(&self, row: usize, scratch: &mut IntRowScratch, out: &mut [i16]) {
        scratch
            .0
            .fill_row_sums(self.parts.iter().copied(), row, self.left_size);
        for (cost, &sum) in out.iter_mut().zip(&scratch.0.sums) {
            *cost = int_cost(sum, self.cost_factor, self.shift);
        }
    }

    pub(crate) fn render_row(&self, row: usize, scratch: &mut IntRowScratch, out: &mut [i16]) {
        self.render_computed_row(row, scratch, out);
        let first = row * self.left_size;
        let begin = self
            .exception_cells
            .partition_point(|&cell| (cell as usize) < first);
        let end = self
            .exception_cells
            .partition_point(|&cell| (cell as usize) < first + self.left_size);
        for (&cell, &value) in self.exception_cells[begin..end]
            .iter()
            .zip(&self.exception_values[begin..end])
        {
            out[cell as usize - first] = value;
        }
    }

    pub fn scratch(&self) -> IntRowScratch {
        IntRowScratch(RowScratch::new(self.parts.iter().copied(), self.left_size))
    }

    pub(crate) fn write_hot_matrix(
        &self,
        shape: &HotShape,
        threads: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        shape.write_matrix_in_order(
            self.left_size,
            self.right_size,
            threads,
            output,
            || (self.scratch(), vec![0i16; self.left_size]),
            |(scratch, row_costs), rows| {
                let mut bytes = vec![0u8; shape.rows_bytes(rows.clone())];
                let mut unwritten = bytes.as_mut_slice();
                for row in rows {
                    let length = shape.row_length(row);
                    if length == 0 {
                        continue;
                    }
                    self.render_row(row, scratch, row_costs);
                    let (written, rest) = unwritten.split_at_mut(length * size_of::<i16>());
                    unwritten = rest;
                    let cells = written.as_chunks_mut::<2>().0.iter_mut();
                    for (cell, cost) in cells.zip(&row_costs[..length]) {
                        *cell = cost.to_le_bytes();
                    }
                }
                bytes
            },
        )
    }
}

impl fmt::Debug for IntRowScratch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IntRowScratch")
            .finish_non_exhaustive()
    }
}

pub struct IntRowScratch(RowScratch<i32>);

impl IntTemplates {
    pub fn view(&self) -> IntView<'_> {
        IntView {
            cost_factor: self.cost_factor,
            shift: self.shift,
            left_size: self.left_size,
            right_size: self.right_size,
            parts: self.parts.iter().map(IntPart::slices).collect(),
            exception_cells: &self.exception_cells,
            exception_values: &self.exception_values,
        }
    }
}
