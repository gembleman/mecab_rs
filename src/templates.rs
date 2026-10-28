use std::io::{self, Write};
use std::mem::size_of;
use std::path::Path;

use crate::error::Error;
use crate::part::{PartSlices, RowScratch, TemplatePart, total_of};
use crate::section::{Plain, Section};

pub(crate) const TEMPLATE_HEADER_PEEK_BYTES: usize = 24;
const SIZES_OFFSET: usize = 16;
const WORD_BYTES: usize = 4;
const ARRAY_ALIGNMENT: usize = 8;
const MIN_COST: f64 = -32767.0;
const MAX_COST: f64 = 32767.0;

pub(crate) fn header_sizes(header: &[u8]) -> Option<(usize, usize)> {
    let word = |at: usize| {
        let bytes = header.get(at..at + WORD_BYTES)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?) as usize)
    };
    Some((word(SIZES_OFFSET)?, word(SIZES_OFFSET + WORD_BYTES)?))
}

pub struct ConnectionTemplates {
    cost_factor: f64,
    left_size: usize,
    right_size: usize,
    parts: Vec<TemplatePart>,
}

fn is_strictly_sorted(part: &TemplatePart) -> bool {
    part.slices().has_sorted_spans()
}

fn sort_spans(part: &mut TemplatePart) {
    let mut left_index = Vec::with_capacity(part.left_index.len());
    let mut weight = Vec::with_capacity(part.weight.len());
    let mut indptr = Vec::with_capacity(part.indptr.len());
    indptr.push(0);
    for span in part.indptr.windows(2) {
        let range = span[0] as usize..span[1] as usize;
        let mut cells: Vec<(u16, f64)> = part.left_index[range.clone()]
            .iter()
            .copied()
            .zip(part.weight[range].iter().copied())
            .collect();
        cells.reverse();
        cells.sort_by_key(|&(index, _)| index);
        cells.dedup_by_key(|&mut (index, _)| index);
        for (index, value) in cells {
            left_index.push(index);
            weight.push(value);
        }
        indptr.push(left_index.len() as u32);
    }
    part.left_index = left_index;
    part.weight = weight;
    part.indptr = indptr;
}

fn validate(part: &TemplatePart, left_size: usize, right_size: usize) -> Result<(), Error> {
    let slices = part.slices();
    let consistent = slices.has_sizes(left_size, right_size) && slices.is_consistent();
    if consistent {
        Ok(())
    } else {
        Err(Error::Corrupt("a connection template is inconsistent"))
    }
}

pub fn to_cost(sum: f64, cost_factor: f64) -> i16 {
    (-cost_factor * sum).clamp(MIN_COST, MAX_COST) as i16
}

fn connection_cost<'a>(
    parts: impl Iterator<Item = PartSlices<'a, f64>>,
    cost_factor: f64,
    left_node_rc: u16,
    right_node_lc: u16,
) -> i16 {
    to_cost(total_of(parts, left_node_rc, right_node_lc), cost_factor)
}

fn padding_after(bytes: usize) -> usize {
    bytes.next_multiple_of(ARRAY_ALIGNMENT) - bytes
}

pub(crate) fn write_array<T: Copy, const WIDTH: usize>(
    output: &mut impl Write,
    items: &[T],
    encode: fn(T) -> [u8; WIDTH],
) -> io::Result<()> {
    for item in items {
        output.write_all(&encode(*item))?;
    }
    output.write_all(&[0; ARRAY_ALIGNMENT][..padding_after(items.len() * WIDTH)])
}

pub(crate) struct TemplateReader<'a> {
    pub(crate) file: &'a [u8],
    path: &'a Path,
    offset: usize,
}

impl<'a> TemplateReader<'a> {
    pub(crate) fn new(file: &'a [u8], path: &'a Path) -> Self {
        Self {
            file,
            path,
            offset: 0,
        }
    }

    pub(crate) fn at_end(&self) -> bool {
        self.offset == self.file.len()
    }

    pub(crate) fn broken(&self, reason: &'static str) -> Error {
        Error::Format {
            file: self.path.to_path_buf(),
            reason,
        }
    }

    pub(crate) fn bytes<const WIDTH: usize>(&mut self) -> Result<[u8; WIDTH], Error> {
        let end = self.offset + WIDTH;
        let bytes = self
            .file
            .get(self.offset..end)
            .ok_or_else(|| self.broken("the file ends inside a header"))?;
        self.offset = end;
        Ok(bytes.try_into().expect("the slice has the requested width"))
    }

    pub(crate) fn count(&mut self) -> Result<usize, Error> {
        self.bytes().map(|bytes| u32::from_le_bytes(bytes) as usize)
    }

    pub(crate) fn copied<T, const WIDTH: usize>(
        &mut self,
        count: usize,
        decode: fn([u8; WIDTH]) -> T,
    ) -> Result<Vec<T>, Error> {
        let length = count
            .checked_mul(WIDTH)
            .ok_or_else(|| self.broken("an array is too long"))?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| self.broken("an array is too long"))?;
        let bytes = self
            .file
            .get(self.offset..end)
            .ok_or_else(|| self.broken("the file ends inside an array"))?;
        let items = bytes
            .as_chunks::<WIDTH>()
            .0
            .iter()
            .map(|&item| decode(item))
            .collect();
        self.offset = end + padding_after(length);
        Ok(items)
    }

    pub(crate) fn section<T: Plain>(&mut self, count: usize) -> Result<Section<T>, Error> {
        let length = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| self.broken("an array is too long"))?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| self.broken("an array is too long"))?;
        let section = Section::new(self.file, self.offset..end)
            .ok_or_else(|| self.broken("an array is misplaced"))?;
        self.offset = end + padding_after(length);
        Ok(section)
    }
}

impl ConnectionTemplates {
    pub fn new(
        cost_factor: f64,
        left_size: usize,
        right_size: usize,
        mut parts: Vec<TemplatePart>,
    ) -> Result<Self, Error> {
        if left_size == 0 || right_size == 0 || left_size > 0xFFFF || right_size > 0xFFFF {
            return Err(Error::Corrupt("the template matrix size is out of range"));
        }
        for part in &mut parts {
            validate(part, left_size, right_size)?;
            if !is_strictly_sorted(part) {
                sort_spans(part);
            }
        }
        Ok(Self {
            cost_factor,
            left_size,
            right_size,
            parts,
        })
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn cost_factor(&self) -> f64 {
        self.cost_factor
    }

    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    fn slices(&self) -> impl Iterator<Item = PartSlices<'_, f64>> {
        self.parts.iter().map(TemplatePart::slices)
    }

    pub fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        connection_cost(self.slices(), self.cost_factor, left_node_rc, right_node_lc)
    }
}

pub struct ExactRows<'a> {
    templates: &'a ConnectionTemplates,
    scratch: RowScratch<f64>,
}

impl<'a> ExactRows<'a> {
    pub fn new(templates: &'a ConnectionTemplates) -> Self {
        Self {
            templates,
            scratch: RowScratch::new(templates.slices(), templates.left_size),
        }
    }

    pub fn render(&mut self, right_left_id: usize, out: &mut [i16]) {
        let left_size = self.templates.left_size;
        self.scratch
            .fill_row_sums(self.templates.slices(), right_left_id, left_size);
        for (cost, sum) in out.iter_mut().zip(&self.scratch.sums) {
            *cost = to_cost(*sum, self.templates.cost_factor);
        }
    }
}
