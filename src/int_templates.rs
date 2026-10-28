use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use memmap2::Mmap;

use crate::clusters::{
    ClusterPlan, ClusterShape, MappedClusters, fill_cells, interleaved_ids, left_ids,
    plan_clusters, right_ids, write_cluster_layout,
};
use crate::dictionary::map_file;
use crate::error::Error;
use crate::hot_shape::{CELL_SUM_BOUND, HotShape, MAX_KEYS};
#[cfg(feature = "xz")]
use crate::int_runtime::runtime_form_from;
use crate::part::{IntPart, MappedPart, PartSlices, RowScratch, total_of};
use crate::section::{Section, SectionSource};
use crate::templates::{TemplateReader, write_array};

pub(crate) const MAGIC: &[u8; 4] = b"MXIT";
const VERSION: u32 = 1;
const MAX_PARTS: usize = 256;
pub const DEFAULT_CLUSTER_COUNT: usize = 12;
const MAX_COST: i64 = 32767;
pub const MAX_SHIFT: u32 = 40;
pub const MAX_COST_FACTOR: u32 = 1 << 24;

pub fn int_cost(total: i64, cost_factor: u32, shift: u32) -> i16 {
    let scaled = -i64::from(cost_factor) * total;
    let cost = if scaled >= 0 {
        scaled >> shift
    } else {
        -((-scaled) >> shift)
    };
    cost.clamp(-MAX_COST, MAX_COST) as i16
}

impl IntPart {
    pub fn compacted(&self) -> Self {
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
            weight: self.weight.clone(),
        }
    }
}

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

fn exception_of(cells: &[u32], values: &[i16], cell: usize) -> Option<i16> {
    let found = cells.binary_search(&u32::try_from(cell).ok()?).ok()?;
    Some(values[found])
}

fn validate<'a>(
    parts: impl Iterator<Item = PartSlices<'a, i32>>,
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    exception_cells: &[u32],
) -> Result<(), Error> {
    let corrupt = Error::Corrupt("the integer templates are inconsistent");
    if left_size == 0 || right_size == 0 || left_size > MAX_KEYS || right_size > MAX_KEYS {
        return Err(Error::Corrupt("the template matrix size is out of range"));
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
    let cells = left_size * right_size;
    let sorted = exception_cells.windows(2).all(|pair| pair[0] < pair[1]);
    let inside = exception_cells
        .last()
        .is_none_or(|&last| (last as usize) < cells);
    if sorted && inside {
        Ok(())
    } else {
        Err(corrupt)
    }
}

pub(crate) struct IntView<'a> {
    pub(crate) cost_factor: u32,
    pub(crate) shift: u32,
    pub(crate) left_size: usize,
    pub(crate) right_size: usize,
    parts: Vec<PartSlices<'a, i32>>,
    exception_cells: &'a [u32],
    exception_values: &'a [i16],
}

impl IntView<'_> {
    pub(crate) fn render_computed_row(
        &self,
        row: usize,
        scratch: &mut IntRowScratch,
        out: &mut [i16],
    ) {
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

    pub(crate) fn scratch(&self) -> IntRowScratch {
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
                let mut bytes = Vec::new();
                for row in rows {
                    let length = shape.row_length(row);
                    if length == 0 {
                        continue;
                    }
                    self.render_row(row, scratch, row_costs);
                    for cost in &row_costs[..length] {
                        bytes.extend_from_slice(&cost.to_le_bytes());
                    }
                }
                bytes
            },
        )
    }
}

pub(crate) struct IntRowScratch(RowScratch<i32>);

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
        exceptions: Vec<(u32, i16)>,
    ) -> Result<Self, Error> {
        let (exception_cells, exception_values): (Vec<u32>, Vec<i16>) =
            exceptions.into_iter().unzip();
        validate(
            parts.iter().map(IntPart::slices),
            cost_factor,
            shift,
            left_size,
            right_size,
            &exception_cells,
        )?;
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
        if reader.bytes::<4>()? != *MAGIC || u32::from_le_bytes(reader.bytes()?) != VERSION {
            return Err(reader.broken("the file is not an integer template file of this version"));
        }
        let cost_factor = reader.count()? as u32;
        let shift = reader.count()? as u32;
        let left_size = reader.count()?;
        let right_size = reader.count()?;
        let part_count = reader.count()?;
        let exception_count = reader.count()?;
        reader.count()?;
        if left_size == 0 || right_size == 0 || left_size > MAX_KEYS || right_size > MAX_KEYS {
            return Err(reader.broken("the template matrix size is out of range"));
        }
        if part_count > MAX_PARTS {
            return Err(reader.broken("too many template parts"));
        }
        let mut parts = Vec::with_capacity(part_count);
        for _ in 0..part_count {
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
        let cells = reader.copied(exception_count, u32::from_le_bytes)?;
        let values = reader.copied(exception_count, i16::from_le_bytes)?;
        Self::new(
            cost_factor,
            shift,
            left_size,
            right_size,
            parts,
            cells.into_iter().zip(values).collect(),
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

    pub fn with_exceptions(&self, exceptions: Vec<(u32, i16)>) -> Result<Self, Error> {
        Self::new(
            self.cost_factor,
            self.shift,
            self.left_size,
            self.right_size,
            self.parts.clone(),
            exceptions,
        )
    }

    pub fn computed_cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let total = total_of(
            self.parts.iter().map(IntPart::slices),
            left_node_rc,
            right_node_lc,
        );
        int_cost(total, self.cost_factor, self.shift)
    }

    pub fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let cell = usize::from(right_node_lc) * self.left_size + usize::from(left_node_rc);
        exception_of(&self.exception_cells, &self.exception_values, cell)
            .unwrap_or_else(|| self.computed_cost(left_node_rc, right_node_lc))
    }

    pub(crate) fn view(&self) -> IntView<'_> {
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

    #[cfg(feature = "xz")]
    pub(crate) fn runtime_form(
        &self,
        shape: &HotShape,
        cluster_count: usize,
        threads: usize,
    ) -> Result<Vec<u8>, Error> {
        let slices: Vec<PartSlices<i32>> = self.parts.iter().map(IntPart::slices).collect();
        let plans = cluster_plans(&slices, cluster_count);
        let data = cluster_data(&slices, &plans, self.left_size, self.right_size, threads)
            .map_err(|_| Error::Unsupported("a cluster sum that does not fit an i32"))?;
        let inputs = RuntimeInputs {
            cost_factor: self.cost_factor,
            shift: self.shift,
            left_size: self.left_size,
            right_size: self.right_size,
            shapes: &data.shapes,
            left_keys: &data.left_keys,
            right_keys: &data.right_keys,
            tables: &data.tables,
            exception_cells: &self.exception_cells,
            exception_values: &self.exception_values,
        };
        runtime_form_from(&inputs, shape)
    }
}

fn cluster_plans(slices: &[PartSlices<i32>], cluster_count: usize) -> Vec<ClusterPlan> {
    if cluster_count == 0 {
        return Vec::new();
    }
    plan_clusters(slices, cluster_count)
}

fn fill_int_table(
    plan: &ClusterPlan,
    parts: &[PartSlices<i32>],
    threads: usize,
) -> io::Result<Vec<i32>> {
    let overflow = AtomicBool::new(false);
    let table = fill_cells(plan, threads, |left, right| {
        let total = total_of(plan.parts.iter().map(|&p| parts[p]), left, right);
        i32::try_from(total).unwrap_or_else(|_| {
            overflow.store(true, Ordering::Relaxed);
            0
        })
    });
    if overflow.into_inner() {
        return Err(io::Error::other(
            "a cluster sum does not fit an i32; use a coarser grid or more clusters",
        ));
    }
    Ok(table)
}

struct ClusterData {
    shapes: Vec<ClusterShape>,
    left_keys: Vec<u16>,
    right_keys: Vec<u16>,
    tables: Vec<i32>,
}

fn cluster_data(
    parts: &[PartSlices<i32>],
    plans: &[ClusterPlan],
    left_size: usize,
    right_size: usize,
    threads: usize,
) -> io::Result<ClusterData> {
    let mut table_start = 0;
    let shapes = plans
        .iter()
        .map(|plan| {
            let shape = ClusterShape {
                table_start,
                left_keys: plan.left_keys,
                right_keys: plan.right_keys,
            };
            table_start += plan.cells();
            shape
        })
        .collect();
    let mut tables = Vec::with_capacity(table_start);
    for plan in plans {
        tables.extend(fill_int_table(plan, parts, threads)?);
    }
    Ok(ClusterData {
        shapes,
        left_keys: interleaved_ids(plans, left_size, left_ids),
        right_keys: interleaved_ids(plans, right_size, right_ids),
        tables,
    })
}

fn write_clusters(data: &ClusterData, output: &mut impl Write) -> io::Result<()> {
    write_cluster_layout(
        output,
        data.shapes
            .iter()
            .map(|shape| (shape.left_keys, shape.right_keys)),
        &data.left_keys,
        &data.right_keys,
        data.tables.len(),
    )?;
    write_array(output, &data.tables, i32::to_le_bytes)
}

pub(crate) struct RuntimeInputs<'a> {
    pub(crate) cost_factor: u32,
    pub(crate) shift: u32,
    pub(crate) left_size: usize,
    pub(crate) right_size: usize,
    pub(crate) shapes: &'a [ClusterShape],
    pub(crate) left_keys: &'a [u16],
    pub(crate) right_keys: &'a [u16],
    pub(crate) tables: &'a [i32],
    pub(crate) exception_cells: &'a [u32],
    pub(crate) exception_values: &'a [i16],
}

pub struct MappedIntTemplates<S = Mmap> {
    map: S,
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    parts: Vec<MappedPart<i32>>,
    exception_cells: Section<u32>,
    exception_values: Section<i16>,
    clusters: Option<MappedClusters<i32>>,
    exception_rows: Vec<u32>,
}

impl MappedIntTemplates<Mmap> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::from_source(map_file(path)?, path)
    }
}

#[allow(private_bounds)]
impl<S: SectionSource> MappedIntTemplates<S> {
    pub(crate) fn from_source(map: S, path: &Path) -> Result<Self, Error> {
        let mut reader = TemplateReader::new(map.all_bytes(), path);
        if reader.bytes::<4>()? != *MAGIC || u32::from_le_bytes(reader.bytes()?) != VERSION {
            return Err(reader.broken("the file is not an integer template file of this version"));
        }
        let cost_factor = reader.count()? as u32;
        let shift = reader.count()? as u32;
        let left_size = reader.count()?;
        let right_size = reader.count()?;
        let part_count = reader.count()?;
        let exception_count = reader.count()?;
        let cluster_count = reader.count()?;
        if left_size == 0 || right_size == 0 || left_size > MAX_KEYS || right_size > MAX_KEYS {
            return Err(reader.broken("the template matrix size is out of range"));
        }
        let mut parts = Vec::with_capacity(part_count.min(MAX_PARTS));
        for _ in 0..part_count {
            parts.push(MappedPart::read(&mut reader, left_size, right_size)?);
        }
        let exception_cells = reader.section(exception_count)?;
        let exception_values = reader.section(exception_count)?;
        let clusters = match cluster_count {
            0 => None,
            count => Some(MappedClusters::read(
                &mut reader,
                count,
                left_size,
                right_size,
            )?),
        };
        if !reader.at_end() {
            return Err(reader.broken("the file has trailing bytes"));
        }
        validate(
            parts.iter().map(|part| part.slices(&map)),
            cost_factor,
            shift,
            left_size,
            right_size,
            exception_cells.view_source(&map),
        )
        .map_err(|_| reader.broken("the integer templates are inconsistent"))?;
        let mut exception_rows = Vec::with_capacity(right_size + 1);
        let mut next = 0usize;
        for row in 0..=right_size {
            let first_of_row = row * left_size;
            let cells = exception_cells.view_source(&map);
            while next < cells.len() && (cells[next] as usize) < first_of_row {
                next += 1;
            }
            exception_rows.push(next as u32);
        }
        Ok(Self {
            map,
            cost_factor,
            shift,
            left_size,
            right_size,
            parts,
            exception_cells,
            exception_values,
            clusters,
            exception_rows,
        })
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub(crate) fn runtime_inputs(&self) -> Option<RuntimeInputs<'_>> {
        let clusters = self.clusters.as_ref()?;
        Some(RuntimeInputs {
            cost_factor: self.cost_factor,
            shift: self.shift,
            left_size: self.left_size,
            right_size: self.right_size,
            shapes: &clusters.shapes,
            left_keys: clusters.left_keys.view_source(&self.map),
            right_keys: clusters.right_keys.view_source(&self.map),
            tables: clusters.tables.view_source(&self.map),
            exception_cells: self.exception_cells.view_source(&self.map),
            exception_values: self.exception_values.view_source(&self.map),
        })
    }

    pub fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        self.cost_with(left_node_rc, right_node_lc, true)
    }

    pub fn cost_from_templates(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        self.cost_with(left_node_rc, right_node_lc, false)
    }

    fn cost_with(&self, left_node_rc: u16, right_node_lc: u16, use_clusters: bool) -> i16 {
        let row = usize::from(right_node_lc);
        let cell = row * self.left_size + usize::from(left_node_rc);
        let span = self.exception_rows[row] as usize..self.exception_rows[row + 1] as usize;
        let exception = if span.is_empty() {
            None
        } else {
            let cells = &self.exception_cells.view_source(&self.map)[span.clone()];
            let found = cells.binary_search(&(cell as u32)).ok();
            found.map(|at| self.exception_values.view_source(&self.map)[span.start + at])
        };
        exception.unwrap_or_else(|| {
            let total = match &self.clusters {
                Some(clusters) if use_clusters => {
                    clusters.sum(&self.map, left_node_rc, right_node_lc)
                }
                _ => total_of(
                    self.parts.iter().map(|part| part.slices(&self.map)),
                    left_node_rc,
                    right_node_lc,
                ),
            };
            int_cost(total, self.cost_factor, self.shift)
        })
    }

}
