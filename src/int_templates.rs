use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use memmap2::Mmap;

use crate::clusters::{ClusterPlan, plan_projection_clusters, representatives};
use crate::dictionary::map_file;
use crate::error::Error;
use crate::hot_shape::HotShape;
use crate::section::{Section, SectionSource};
use crate::templates::{TemplateReader, write_array};

pub(crate) const MAGIC: &[u8; 4] = b"MXIT";
const VERSION: u32 = 1;
const MAX_PARTS: usize = 256;
const MAX_SHAPES: usize = 256;
const ROWS_PER_CHUNK: usize = 16;
pub const DEFAULT_CLUSTER_COUNT: usize = 12;
const ROWS_PER_BLOCK: usize = 64;
const MAX_COST: i64 = 32767;
const MAX_KEYS: usize = 0xFFFF;
const CELL_SUM_BOUND: u128 = 1 << 62;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntPart {
    pub left_count: usize,
    pub right_count: usize,
    pub left_projection: Vec<u16>,
    pub right_projection: Vec<u16>,
    pub indptr: Vec<u32>,
    pub left_index: Vec<u16>,
    pub weight: Vec<i32>,
}

impl IntPart {
    pub(crate) fn slices(&self) -> IntSlices<'_> {
        IntSlices {
            left_count: self.left_count,
            right_count: self.right_count,
            left_projection: &self.left_projection,
            right_projection: &self.right_projection,
            indptr: &self.indptr,
            left_index: &self.left_index,
            weight: &self.weight,
        }
    }

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

#[derive(Clone, Copy)]
pub(crate) struct IntSlices<'a> {
    left_count: usize,
    right_count: usize,
    left_projection: &'a [u16],
    right_projection: &'a [u16],
    indptr: &'a [u32],
    left_index: &'a [u16],
    weight: &'a [i32],
}

impl IntSlices<'_> {
    #[inline]
    fn weight_of(&self, left_node_rc: u16, right_node_lc: u16) -> Option<i32> {
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

    fn is_consistent(&self, left_size: usize, right_size: usize) -> bool {
        let nnz = self.left_index.len();
        self.left_projection.len() == left_size
            && self.right_projection.len() == right_size
            && self.weight.len() == nnz
            && self.indptr.len() == self.right_count + 1
            && self.indptr.first() == Some(&0)
            && self.indptr.last().is_some_and(|&end| end as usize == nnz)
            && self.indptr.windows(2).all(|span| {
                span[0] <= span[1]
                    && self.left_index[span[0] as usize..span[1] as usize]
                        .windows(2)
                        .all(|pair| pair[0] < pair[1])
            })
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

    fn max_weight(&self) -> u64 {
        self.weight
            .iter()
            .map(|weight| u64::from(weight.unsigned_abs()))
            .max()
            .unwrap_or(0)
    }
}

fn total_of<'a>(
    parts: impl Iterator<Item = IntSlices<'a>>,
    left_node_rc: u16,
    right_node_lc: u16,
) -> i64 {
    parts
        .filter_map(|part| part.weight_of(left_node_rc, right_node_lc))
        .map(i64::from)
        .sum()
}

fn exception_of(cells: &[u32], values: &[i16], cell: usize) -> Option<i16> {
    let found = cells.binary_search(&u32::try_from(cell).ok()?).ok()?;
    Some(values[found])
}

fn validate<'a>(
    parts: impl Iterator<Item = IntSlices<'a>>,
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
        if !part.is_consistent(left_size, right_size) {
            return Err(corrupt);
        }
        magnitude += part.max_weight();
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

struct RowScratch {
    sums: Vec<i64>,
    lookups: Vec<Vec<i32>>,
}

impl RowScratch {
    fn new(parts: &[IntSlices], left_size: usize) -> Self {
        Self {
            sums: vec![0; left_size],
            lookups: parts
                .iter()
                .map(|part| vec![0; part.left_count + 1])
                .collect(),
        }
    }
}

pub(crate) struct IntView<'a> {
    pub(crate) cost_factor: u32,
    pub(crate) shift: u32,
    pub(crate) left_size: usize,
    pub(crate) right_size: usize,
    parts: Vec<IntSlices<'a>>,
    exception_cells: &'a [u32],
    exception_values: &'a [i16],
}

impl IntView<'_> {
    fn fill_row_sums(&self, row: usize, scratch: &mut RowScratch) {
        scratch.sums.fill(0);
        for (part, lookup) in self.parts.iter().zip(&mut scratch.lookups) {
            let right_key = usize::from(part.right_projection[row]);
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
            for (sum, projected) in scratch.sums.iter_mut().zip(part.left_projection) {
                *sum += i64::from(lookup[usize::from(*projected)]);
            }
            for index in left_index {
                lookup[usize::from(*index)] = 0;
            }
        }
    }

    pub(crate) fn render_computed_row(
        &self,
        row: usize,
        scratch: &mut IntRowScratch,
        out: &mut [i16],
    ) {
        self.fill_row_sums(row, &mut scratch.0);
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
        IntRowScratch(RowScratch::new(&self.parts, self.left_size))
    }

    pub(crate) fn write_hot_matrix(
        &self,
        shape: &HotShape,
        threads: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        shape.write_header(self.left_size, self.right_size, output)?;
        let blocks = shape.rows().div_ceil(ROWS_PER_BLOCK);
        let next_block = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::sync_channel::<(usize, Vec<u8>)>(threads.max(1) * 2);
        thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                let sender = sender.clone();
                let next_block = &next_block;
                scope.spawn(move || {
                    let mut scratch = self.scratch();
                    let mut row_costs = vec![0i16; self.left_size];
                    loop {
                        let block = next_block.fetch_add(1, Ordering::Relaxed);
                        if block >= blocks {
                            break;
                        }
                        let first = block * ROWS_PER_BLOCK;
                        let last = (first + ROWS_PER_BLOCK).min(shape.rows());
                        let mut bytes = Vec::new();
                        for row in first..last {
                            let length = shape.row_length(row);
                            if length == 0 {
                                continue;
                            }
                            self.render_row(row, &mut scratch, &mut row_costs);
                            for cost in &row_costs[..length] {
                                bytes.extend_from_slice(&cost.to_le_bytes());
                            }
                        }
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
}

pub(crate) struct IntRowScratch(RowScratch);

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

    pub fn visit_rows(&self, first: usize, end: usize, mut visit: impl FnMut(usize, &[i16])) {
        let view = self.view();
        let mut scratch = view.scratch();
        let mut costs = vec![0i16; self.left_size];
        for row in first..end.min(self.right_size) {
            view.render_row(row, &mut scratch, &mut costs);
            visit(row, &costs);
        }
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
        let slices: Vec<IntSlices> = self.parts.iter().map(IntPart::slices).collect();
        let plans = if cluster_count == 0 {
            Vec::new()
        } else {
            let projections: Vec<(&[u16], &[u16])> = slices
                .iter()
                .map(|part| (part.left_projection, part.right_projection))
                .collect();
            plan_projection_clusters(&projections, cluster_count)
        };
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
            for count in [part.left_count, part.right_count, part.weight.len(), 0] {
                output.write_all(&(count as u32).to_le_bytes())?;
            }
            write_array(output, &part.left_projection, u16::to_le_bytes)?;
            write_array(output, &part.right_projection, u16::to_le_bytes)?;
            write_array(output, &part.indptr, u32::to_le_bytes)?;
            write_array(output, &part.left_index, u16::to_le_bytes)?;
            write_array(output, &part.weight, i32::to_le_bytes)?;
        }
        write_array(output, &self.exception_cells, u32::to_le_bytes)?;
        write_array(output, &self.exception_values, i16::to_le_bytes)?;
        if plans.is_empty() {
            return Ok(());
        }
        write_clusters(
            &slices,
            &plans,
            self.left_size,
            self.right_size,
            threads,
            output,
        )
    }
}

fn fill_int_table(plan: &ClusterPlan, parts: &[IntSlices], threads: usize) -> io::Result<Vec<i32>> {
    let lefts = representatives(&plan.left_ids, plan.left_keys);
    let rights = representatives(&plan.right_ids, plan.right_keys);
    let mut table = vec![0i32; plan.cells()];
    let chunks = Mutex::new(
        table
            .chunks_mut(ROWS_PER_CHUNK * plan.left_keys)
            .enumerate(),
    );
    let overflow = AtomicBool::new(false);
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let next = chunks.lock().expect("a table worker panicked").next();
                    let Some((chunk, cells)) = next else { break };
                    for (row, cells) in cells.chunks_mut(plan.left_keys).enumerate() {
                        let right = rights[chunk * ROWS_PER_CHUNK + row];
                        for (cell, &left) in cells.iter_mut().zip(&lefts) {
                            let total = plan
                                .parts
                                .iter()
                                .filter_map(|&p| parts[p].weight_of(left, right))
                                .map(i64::from)
                                .sum::<i64>();
                            match i32::try_from(total) {
                                Ok(total) => *cell = total,
                                Err(_) => overflow.store(true, Ordering::Relaxed),
                            }
                        }
                    }
                }
            });
        }
    });
    if overflow.into_inner() {
        return Err(io::Error::other(
            "a cluster sum does not fit an i32; use a coarser grid or more clusters",
        ));
    }
    Ok(table)
}

fn write_clusters(
    parts: &[IntSlices],
    plans: &[ClusterPlan],
    left_size: usize,
    right_size: usize,
    threads: usize,
    output: &mut impl Write,
) -> io::Result<()> {
    for plan in plans {
        for keys in [plan.left_keys, plan.right_keys] {
            output.write_all(&(keys as u32).to_le_bytes())?;
        }
    }
    for (size, ids_of) in [
        (
            left_size,
            (|plan| &plan.left_ids) as fn(&ClusterPlan) -> &Vec<u16>,
        ),
        (right_size, |plan| &plan.right_ids),
    ] {
        let interleaved: Vec<u16> = (0..size)
            .flat_map(|index| plans.iter().map(move |plan| ids_of(plan)[index]))
            .collect();
        write_array(output, &interleaved, u16::to_le_bytes)?;
    }
    let total: usize = plans.iter().map(ClusterPlan::cells).sum();
    output.write_all(&(total as u32).to_le_bytes())?;
    output.write_all(&[0; 4])?;
    let mut cells = Vec::with_capacity(total);
    for plan in plans {
        cells.extend(fill_int_table(plan, parts, threads)?);
    }
    write_array(output, &cells, i32::to_le_bytes)?;
    Ok(())
}

fn keys_fit(keys: &[u16], limits: &[usize]) -> bool {
    keys.chunks_exact(limits.len()).all(|row| {
        row.iter()
            .zip(limits)
            .all(|(&key, &limit)| usize::from(key) < limit)
    })
}

pub(crate) struct ClusterShape {
    table_start: usize,
    pub(crate) left_keys: usize,
    pub(crate) right_keys: usize,
}

struct MappedIntClusters {
    shapes: Vec<ClusterShape>,
    left_keys: Section<u16>,
    right_keys: Section<u16>,
    tables: Section<i32>,
}

impl MappedIntClusters {
    fn read(
        reader: &mut TemplateReader,
        count: usize,
        left_size: usize,
        right_size: usize,
    ) -> Result<Self, Error> {
        let mut shapes = Vec::with_capacity(count.min(MAX_SHAPES));
        let mut left_limits = Vec::with_capacity(count.min(MAX_SHAPES));
        let mut right_limits = Vec::with_capacity(count.min(MAX_SHAPES));
        let mut table_cells = 0usize;
        for _ in 0..count {
            let left_keys = reader.count()?;
            let right_keys = reader.count()?;
            shapes.push(ClusterShape {
                table_start: table_cells,
                left_keys,
                right_keys,
            });
            left_limits.push(left_keys);
            right_limits.push(right_keys);
            table_cells = left_keys
                .checked_mul(right_keys)
                .and_then(|cells| table_cells.checked_add(cells))
                .ok_or_else(|| reader.broken("a cluster table is too large"))?;
        }
        let left_keys = reader.section(left_size * count)?;
        let right_keys = reader.section(right_size * count)?;
        let declared = reader.count()?;
        reader.count()?;
        if declared != table_cells {
            return Err(reader.broken("the cluster tables do not match their shapes"));
        }
        let tables = reader.section(table_cells)?;
        let valid = keys_fit(left_keys.view(reader.file), &left_limits)
            && keys_fit(right_keys.view(reader.file), &right_limits);
        if valid {
            Ok(Self {
                shapes,
                left_keys,
                right_keys,
                tables,
            })
        } else {
            Err(reader.broken("a cluster key is out of range"))
        }
    }

    #[inline]
    fn total<S: SectionSource>(&self, file: &S, left_right_id: u16, right_left_id: u16) -> i64 {
        let count = self.shapes.len();
        let left = &self.left_keys.view_source(file)[usize::from(left_right_id) * count..][..count];
        let right =
            &self.right_keys.view_source(file)[usize::from(right_left_id) * count..][..count];
        let tables = self.tables.view_source(file);
        let mut total = 0i64;
        for ((shape, &left), &right) in self.shapes.iter().zip(left).zip(right) {
            total += i64::from(
                tables
                    [shape.table_start + usize::from(right) * shape.left_keys + usize::from(left)],
            );
        }
        total
    }
}

struct MappedIntPart {
    left_count: usize,
    right_count: usize,
    left_projection: Section<u16>,
    right_projection: Section<u16>,
    indptr: Section<u32>,
    left_index: Section<u16>,
    weight: Section<i32>,
}

impl MappedIntPart {
    fn slices<'a, S: SectionSource>(&self, file: &'a S) -> IntSlices<'a> {
        IntSlices {
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
    parts: Vec<MappedIntPart>,
    exception_cells: Section<u32>,
    exception_values: Section<i16>,
    clusters: Option<MappedIntClusters>,
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
            let left_count = reader.count()?;
            let right_count = reader.count()?;
            let nnz = reader.count()?;
            reader.count()?;
            parts.push(MappedIntPart {
                left_count,
                right_count,
                left_projection: reader.section(left_size)?,
                right_projection: reader.section(right_size)?,
                indptr: reader.section(right_count + 1)?,
                left_index: reader.section(nnz)?,
                weight: reader.section(nnz)?,
            });
        }
        let exception_cells = reader.section(exception_count)?;
        let exception_values = reader.section(exception_count)?;
        let clusters = match cluster_count {
            0 => None,
            count => Some(MappedIntClusters::read(
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

    pub fn shift(&self) -> u32 {
        self.shift
    }

    pub fn exception_count(&self) -> usize {
        self.exception_cells.view_source(&self.map).len()
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
                    clusters.total(&self.map, left_node_rc, right_node_lc)
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
