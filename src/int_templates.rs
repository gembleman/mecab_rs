use std::io::{self, Write};
use std::path::Path;

use crate::dict_files::MATRIX_ITP_FILE;
use crate::error::Error;
use crate::hot_shape::{HotShape, MAX_KEYS};
use crate::part::{IntPart, PartSlices, total_of};
use crate::templates::{TemplateReader, write_array};

mod cluster;
mod mapped;
#[cfg(feature = "xz")]
mod runtime;
mod validation;
mod view;

pub(crate) use cluster::RuntimeInputs;
pub use mapped::MappedIntTemplates;

use cluster::{cluster_data, cluster_plans, write_clusters};
use validation::{validate, validate_exception_cells, validate_exception_lengths};

pub(crate) const MAGIC: &[u8; 4] = b"MXIT";
const VERSION: u32 = 1;
const MAX_PARTS: usize = 256;
pub(crate) const DEFAULT_CLUSTER_COUNT: usize = 12;
const MAX_COST: i64 = 32767;
pub(crate) const MAX_SHIFT: u32 = 40;
pub const MAX_COST_FACTOR: u32 = 1 << 24;

pub(crate) fn int_cost(total: i64, cost_factor: u32, shift: u32) -> i16 {
    let scaled = -i64::from(cost_factor) * total;
    let cost = if scaled >= 0 {
        scaled >> shift
    } else {
        -((-scaled) >> shift)
    };
    cost.clamp(-MAX_COST, MAX_COST) as i16
}

impl IntPart {
    #[must_use]
    pub fn compacted(self) -> Self {
        let mut left_used = vec![false; self.left_count];
        for &key in &self.left_index {
            left_used[usize::from(key)] = true;
        }
        let right_used: Vec<bool> = self.indptr.windows(2).map(|s| s[0] < s[1]).collect();
        let rank = |used: &[bool]| -> Vec<u16> {
            let mut next = 0u16;
            used.iter()
                .map(|&used| {
                    let at = next;
                    next += u16::from(used);
                    at
                })
                .collect()
        };
        let (left_rank, right_rank) = (rank(&left_used), rank(&right_used));
        let left_count = left_used.iter().filter(|&&used| used).count();
        let right_count = right_used.iter().filter(|&&used| used).count();
        let remap = |projection: &[u16], used: &[bool], rank: &[u16], count: usize| -> Vec<u16> {
            projection
                .iter()
                .map(|&key| match used.get(usize::from(key)) {
                    Some(true) => rank[usize::from(key)],
                    _ => count as u16,
                })
                .collect()
        };
        let mut indptr = Vec::with_capacity(right_count + 1);
        indptr.push(0);
        for (span, _) in self
            .indptr
            .windows(2)
            .zip(&right_used)
            .filter(|(_, used)| **used)
        {
            indptr.push(span[1] - span[0] + indptr.last().copied().unwrap_or(0));
        }
        Self {
            left_count,
            right_count,
            left_projection: remap(&self.left_projection, &left_used, &left_rank, left_count),
            right_projection: remap(
                &self.right_projection,
                &right_used,
                &right_rank,
                right_count,
            ),
            indptr,
            left_index: self
                .left_index
                .iter()
                .map(|&key| left_rank[usize::from(key)])
                .collect(),
            weight: self.weight,
        }
    }
}

pub(crate) struct TemplateHeader {
    pub(crate) cost_factor: u32,
    pub(crate) shift: u32,
    pub(crate) left_size: usize,
    pub(crate) right_size: usize,
    pub(crate) part_count: usize,
    pub(crate) exception_count: usize,
    pub(crate) cluster_count: usize,
}

fn read_word(reader: &mut TemplateReader) -> Result<u32, Error> {
    u32::try_from(reader.count()?).map_err(|_| reader.broken("a header value is too large"))
}

impl TemplateHeader {
    pub(crate) fn read(reader: &mut TemplateReader) -> Result<Self, Error> {
        if reader.bytes::<4>()? != *MAGIC || u32::from_le_bytes(reader.bytes()?) != VERSION {
            return Err(reader.broken("the file is not an integer template file of this version"));
        }
        let cost_factor = read_word(reader)?;
        let shift = read_word(reader)?;
        let left_size = reader.count()?;
        let right_size = reader.count()?;
        let part_count = reader.count()?;
        let exception_count = reader.count()?;
        let cluster_count = reader.count()?;
        if left_size == 0 || right_size == 0 || left_size > MAX_KEYS || right_size > MAX_KEYS {
            return Err(reader.broken("the template matrix size is out of range"));
        }
        Ok(Self {
            cost_factor,
            shift,
            left_size,
            right_size,
            part_count,
            exception_count,
            cluster_count,
        })
    }
}

fn exception_of(cells: &[u32], values: &[i16], cell: usize) -> Option<i16> {
    let found = cells.binary_search(&u32::try_from(cell).ok()?).ok()?;
    Some(values[found])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntTemplates {
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    parts: Vec<IntPart>,
    exception_cells: Vec<u32>,
    exception_values: Vec<i16>,
}

impl IntTemplates {
    pub fn new(
        cost_factor: u32,
        shift: u32,
        left_size: usize,
        right_size: usize,
        parts: Vec<IntPart>,
        exception_cells: Vec<u32>,
        exception_values: Vec<i16>,
    ) -> Result<Self, Error> {
        validate_exception_lengths(&exception_cells, &exception_values)
            .and_then(|()| {
                validate(
                    parts.iter().map(IntPart::slices),
                    cost_factor,
                    shift,
                    left_size,
                    right_size,
                    &exception_cells,
                )
            })
            .map_err(|error| error.in_file(Path::new(MATRIX_ITP_FILE)))?;
        Ok(Self {
            cost_factor,
            shift,
            left_size,
            right_size,
            parts,
            exception_cells,
            exception_values,
        })
    }

    pub fn from_file_bytes(file: &[u8], path: &Path) -> Result<Self, Error> {
        let mut reader = TemplateReader::new(file, path);
        let header = TemplateHeader::read(&mut reader)?;
        if header.part_count > MAX_PARTS {
            return Err(reader.broken("too many template parts"));
        }
        let (left_size, right_size) = (header.left_size, header.right_size);
        let mut parts = Vec::with_capacity(header.part_count);
        for _ in 0..header.part_count {
            let left_count = reader.count()?;
            let right_count = reader.count()?;
            let entries = reader.count()?;
            reader.count()?;
            let right_bound = right_count
                .checked_add(1)
                .ok_or_else(|| reader.broken("an array is too long"))?;
            parts.push(IntPart {
                left_count,
                right_count,
                left_projection: reader.copied(left_size, u16::from_le_bytes)?,
                right_projection: reader.copied(right_size, u16::from_le_bytes)?,
                indptr: reader.copied(right_bound, u32::from_le_bytes)?,
                left_index: reader.copied(entries, u16::from_le_bytes)?,
                weight: reader.copied(entries, i32::from_le_bytes)?,
            });
        }
        let cells = reader.copied(header.exception_count, u32::from_le_bytes)?;
        let values = reader.copied(header.exception_count, i16::from_le_bytes)?;
        Self::new(
            header.cost_factor,
            header.shift,
            left_size,
            right_size,
            parts,
            cells,
            values,
        )
    }

    pub fn cost_factor(&self) -> u32 {
        self.cost_factor
    }

    pub fn shift(&self) -> u32 {
        self.shift
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn parts(&self) -> &[IntPart] {
        &self.parts
    }

    pub fn exceptions(&self) -> impl ExactSizeIterator<Item = (u32, i16)> + '_ {
        self.exception_cells
            .iter()
            .copied()
            .zip(self.exception_values.iter().copied())
    }

    pub fn exception_count(&self) -> usize {
        self.exception_cells.len()
    }

    pub fn with_exceptions(
        self,
        exception_cells: Vec<u32>,
        exception_values: Vec<i16>,
    ) -> Result<Self, Error> {
        validate_exception_lengths(&exception_cells, &exception_values)
            .and_then(|()| {
                validate_exception_cells(&exception_cells, self.left_size, self.right_size)
            })
            .map_err(|error| error.in_file(Path::new(MATRIX_ITP_FILE)))?;
        Ok(Self {
            exception_cells,
            exception_values,
            ..self
        })
    }

    pub(crate) fn computed_cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let total = total_of(
            self.parts.iter().map(IntPart::slices),
            left_node_rc,
            right_node_lc,
        );
        int_cost(total, self.cost_factor, self.shift)
    }

    pub fn try_cost(&self, left_node_rc: u16, right_node_lc: u16) -> Option<i16> {
        let inside = usize::from(left_node_rc) < self.left_size
            && usize::from(right_node_lc) < self.right_size;
        inside.then(|| self.cost(left_node_rc, right_node_lc))
    }

    fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let cell = usize::from(right_node_lc) * self.left_size + usize::from(left_node_rc);
        exception_of(&self.exception_cells, &self.exception_values, cell)
            .unwrap_or_else(|| self.computed_cost(left_node_rc, right_node_lc))
    }

    pub fn write_hot_matrix(
        &self,
        shape: &HotShape,
        threads: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        self.view().write_hot_matrix(shape, threads, output)
    }

    pub fn write_templates(&self, output: &mut impl Write) -> io::Result<()> {
        self.write_clustered_templates(DEFAULT_CLUSTER_COUNT, 4, output)
    }

    pub fn write_clustered_templates(
        &self,
        cluster_count: usize,
        threads: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let slices: Vec<PartSlices<i32>> = self.parts.iter().map(IntPart::slices).collect();
        let plans = cluster_plans(&slices, cluster_count);
        output.write_all(MAGIC)?;
        output.write_all(&VERSION.to_le_bytes())?;
        for value in [
            self.cost_factor,
            self.shift,
            self.left_size as u32,
            self.right_size as u32,
            self.parts.len() as u32,
            self.exception_cells.len() as u32,
            plans.len() as u32,
        ] {
            output.write_all(&value.to_le_bytes())?;
        }
        for part in &self.parts {
            part.write(output, i32::to_le_bytes)?;
        }
        write_array(output, &self.exception_cells, u32::to_le_bytes)?;
        write_array(output, &self.exception_values, i16::to_le_bytes)?;
        if plans.is_empty() {
            return Ok(());
        }
        let data = cluster_data(&slices, &plans, self.left_size, self.right_size, threads)?;
        write_clusters(&data, output)
    }
}
