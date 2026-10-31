use std::io::{self, Write};
use std::ops::AddAssign;

use crate::error::Error;
use crate::section::{Plain, Section, SectionSource};
use crate::templates::{TemplateReader, write_array};

const RESERVED_COUNT: usize = 0;

pub(crate) trait Weight: Plain + Default + PartialEq {
    type Sum: Copy + Default + AddAssign;

    fn widen(self) -> Self::Sum;
}

impl Weight for i32 {
    type Sum = i64;

    #[inline]
    fn widen(self) -> i64 {
        i64::from(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part<W> {
    pub left_count: usize,
    pub right_count: usize,
    pub left_projection: Vec<u16>,
    pub right_projection: Vec<u16>,
    pub indptr: Vec<u32>,
    pub left_index: Vec<u16>,
    pub weight: Vec<W>,
}

pub type IntPart = Part<i32>;

impl<W: Copy> Part<W> {
    pub(crate) fn slices(&self) -> PartSlices<'_, W> {
        PartSlices {
            left_count: self.left_count,
            right_count: self.right_count,
            left_projection: &self.left_projection,
            right_projection: &self.right_projection,
            indptr: &self.indptr,
            left_index: &self.left_index,
            weight: &self.weight,
        }
    }

    pub(crate) fn write<const WIDTH: usize>(
        &self,
        output: &mut impl Write,
        encode_weight: fn(W) -> [u8; WIDTH],
    ) -> io::Result<()> {
        let counts = [
            self.left_count,
            self.right_count,
            self.weight.len(),
            RESERVED_COUNT,
        ];
        for count in counts {
            output.write_all(&(count as u32).to_le_bytes())?;
        }
        write_array(output, &self.left_projection, u16::to_le_bytes)?;
        write_array(output, &self.right_projection, u16::to_le_bytes)?;
        write_array(output, &self.indptr, u32::to_le_bytes)?;
        write_array(output, &self.left_index, u16::to_le_bytes)?;
        write_array(output, &self.weight, encode_weight)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PartSlices<'a, W> {
    pub(crate) left_count: usize,
    pub(crate) right_count: usize,
    pub(crate) left_projection: &'a [u16],
    pub(crate) right_projection: &'a [u16],
    pub(crate) indptr: &'a [u32],
    pub(crate) left_index: &'a [u16],
    pub(crate) weight: &'a [W],
}

impl<W: Weight> PartSlices<'_, W> {
    #[inline]
    pub(crate) fn weight_of(&self, left_node_rc: u16, right_node_lc: u16) -> Option<W> {
        let right_key = usize::from(self.right_projection[usize::from(right_node_lc)]);
        if right_key == self.right_count {
            return None;
        }
        let left_key = self.left_projection[usize::from(left_node_rc)];
        let span = self.indptr[right_key] as usize..self.indptr[right_key + 1] as usize;
        let found = self.left_index[span.clone()]
            .binary_search(&left_key)
            .ok()?;
        Some(self.weight[span.start + found])
    }

    pub(crate) fn is_consistent(&self) -> bool {
        let nnz = self.left_index.len();
        self.indptr.first() == Some(&0)
            && self.indptr.last().is_some_and(|&end| end as usize == nnz)
            && self.indptr.windows(2).all(|span| span[0] <= span[1])
            && self
                .left_projection
                .iter()
                .all(|&key| usize::from(key) <= self.left_count)
            && self
                .right_projection
                .iter()
                .all(|&key| usize::from(key) <= self.right_count)
            && self
                .left_index
                .iter()
                .all(|&key| usize::from(key) < self.left_count)
    }

    pub(crate) fn has_sorted_spans(&self) -> bool {
        self.indptr.windows(2).all(|span| {
            self.left_index[span[0] as usize..span[1] as usize]
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        })
    }

    pub(crate) fn has_sizes(&self, left_size: usize, right_size: usize) -> bool {
        self.left_projection.len() == left_size
            && self.right_projection.len() == right_size
            && self.weight.len() == self.left_index.len()
            && self.indptr.len() == self.right_count + 1
    }
}

pub(crate) fn total_of<'a, W: Weight + 'a>(
    parts: impl Iterator<Item = PartSlices<'a, W>>,
    left_node_rc: u16,
    right_node_lc: u16,
) -> W::Sum {
    parts
        .filter_map(|part| part.weight_of(left_node_rc, right_node_lc))
        .fold(W::Sum::default(), |mut sum, weight| {
            sum += weight.widen();
            sum
        })
}

pub(crate) struct RowScratch<W: Weight> {
    pub(crate) sums: Vec<W::Sum>,
    lookups: Vec<Vec<W>>,
}

impl<W: Weight> RowScratch<W> {
    pub(crate) fn new<'a>(parts: impl Iterator<Item = PartSlices<'a, W>>, left_size: usize) -> Self
    where
        W: 'a,
    {
        Self {
            sums: vec![W::Sum::default(); left_size],
            lookups: parts
                .map(|part| vec![W::default(); part.left_count + 1])
                .collect(),
        }
    }

    pub(crate) fn fill_row_sums<'a>(
        &mut self,
        parts: impl Iterator<Item = PartSlices<'a, W>>,
        right_left_id: usize,
        hot_left: usize,
    ) where
        W: 'a,
    {
        self.sums[..hot_left].fill(W::Sum::default());
        for (part, lookup) in parts.zip(&mut self.lookups) {
            let right_key = usize::from(part.right_projection[right_left_id]);
            if right_key == part.right_count {
                continue;
            }
            let span = part.indptr[right_key] as usize..part.indptr[right_key + 1] as usize;
            if span.is_empty() {
                continue;
            }
            let left_index = &part.left_index[span.clone()];
            for (index, weight) in left_index.iter().zip(&part.weight[span]) {
                lookup[usize::from(*index)] = *weight;
            }
            for (sum, projected) in self.sums[..hot_left].iter_mut().zip(part.left_projection) {
                *sum += lookup[usize::from(*projected)].widen();
            }
            for index in left_index {
                lookup[usize::from(*index)] = W::default();
            }
        }
    }
}

pub(crate) struct MappedPart<W> {
    left_count: usize,
    right_count: usize,
    left_projection: Section<u16>,
    right_projection: Section<u16>,
    indptr: Section<u32>,
    left_index: Section<u16>,
    weight: Section<W>,
}

impl<W: Weight> MappedPart<W> {
    pub(crate) fn read(
        reader: &mut TemplateReader,
        left_size: usize,
        right_size: usize,
    ) -> Result<Self, Error> {
        let left_count = reader.count()?;
        let right_count = reader.count()?;
        let nnz = reader.count()?;
        reader.count()?;
        Ok(Self {
            left_count,
            right_count,
            left_projection: reader.section(left_size)?,
            right_projection: reader.section(right_size)?,
            indptr: reader.section(right_count + 1)?,
            left_index: reader.section(nnz)?,
            weight: reader.section(nnz)?,
        })
    }

    pub(crate) fn slices<'a, S: SectionSource>(&self, file: &'a S) -> PartSlices<'a, W> {
        PartSlices {
            left_count: self.left_count,
            right_count: self.right_count,
            left_projection: self.left_projection.view_source(file),
            right_projection: self.right_projection.view_source(file),
            indptr: self.indptr.view_source(file),
            left_index: self.left_index.view_source(file),
            weight: self.weight.view_source(file),
        }
    }
}
