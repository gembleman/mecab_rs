use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Mutex;
use std::thread;

use crate::error::Error;
use crate::part::{PartSlices, Weight};
use crate::section::{Section, SectionSource};
use crate::templates::{TemplateReader, write_array};

const ROWS_PER_CHUNK: usize = 16;
const MAX_SHAPES: usize = 256;

pub(crate) struct ClusterPlan {
    id: usize,
    pub(crate) parts: Vec<usize>,
    pub(crate) left_ids: Vec<u16>,
    pub(crate) right_ids: Vec<u16>,
    pub(crate) left_keys: usize,
    pub(crate) right_keys: usize,
}

fn dense_ids(keys: impl Iterator<Item = u32>) -> (Vec<u16>, usize) {
    let mut seen: HashMap<u32, u16> = HashMap::new();
    let ids = keys
        .map(|key| {
            let next = seen.len() as u16;
            *seen.entry(key).or_insert(next)
        })
        .collect();
    (ids, seen.len())
}

fn joined_ids(first: &[u16], second: &[u16]) -> (Vec<u16>, usize) {
    dense_ids(
        first
            .iter()
            .zip(second)
            .map(|(&a, &b)| u32::from(a) << u16::BITS | u32::from(b)),
    )
}

pub(crate) fn representatives(ids: &[u16], keys: usize) -> Vec<u16> {
    let mut first = vec![0; keys];
    for (index, &id) in ids.iter().enumerate().rev() {
        first[usize::from(id)] = index as u16;
    }
    first
}

impl ClusterPlan {
    fn of_projections(id: usize, left_projection: &[u16], right_projection: &[u16]) -> Self {
        let (left_ids, left_keys) = dense_ids(left_projection.iter().map(|&key| u32::from(key)));
        let (right_ids, right_keys) = dense_ids(right_projection.iter().map(|&key| u32::from(key)));
        Self {
            id,
            parts: vec![id],
            left_ids,
            right_ids,
            left_keys,
            right_keys,
        }
    }

    pub(crate) fn cells(&self) -> usize {
        self.left_keys * self.right_keys
    }

    fn merged_cells(&self, other: &Self) -> usize {
        joined_ids(&self.left_ids, &other.left_ids).1
            * joined_ids(&self.right_ids, &other.right_ids).1
    }

    fn merged(&self, id: usize, other: &Self) -> Self {
        let (left_ids, left_keys) = joined_ids(&self.left_ids, &other.left_ids);
        let (right_ids, right_keys) = joined_ids(&self.right_ids, &other.right_ids);
        let mut parts = [self.parts.as_slice(), other.parts.as_slice()].concat();
        parts.sort_unstable();
        Self {
            id,
            parts,
            left_ids,
            right_ids,
            left_keys,
            right_keys,
        }
    }
}

fn growth_of_merge(plans: &[ClusterPlan], first: usize, second: usize) -> i64 {
    let merged = plans[first].merged_cells(&plans[second]) as i64;
    merged - plans[first].cells() as i64 - plans[second].cells() as i64
}

pub(crate) fn plan_clusters<W: Weight>(parts: &[PartSlices<W>], target: usize) -> Vec<ClusterPlan> {
    let projections: Vec<(&[u16], &[u16])> = parts
        .iter()
        .map(|part| (part.left_projection, part.right_projection))
        .collect();
    plan_projection_clusters(&projections, target)
}

pub(crate) fn plan_projection_clusters(
    projections: &[(&[u16], &[u16])],
    target: usize,
) -> Vec<ClusterPlan> {
    let mut plans: Vec<ClusterPlan> = projections
        .iter()
        .enumerate()
        .map(|(index, &(left, right))| ClusterPlan::of_projections(index, left, right))
        .collect();
    let mut next_id = plans.len();
    let mut growth: HashMap<(usize, usize), i64> = HashMap::new();
    while plans.len() > target.max(1) {
        let mut cheapest = (i64::MAX, 0, 1);
        for first in 0..plans.len() {
            for second in first + 1..plans.len() {
                let key = (plans[first].id, plans[second].id);
                let cells = *growth
                    .entry(key)
                    .or_insert_with(|| growth_of_merge(&plans, first, second));
                cheapest = cheapest.min((cells, first, second));
            }
        }
        let (_, first, second) = cheapest;
        let merged = plans[first].merged(next_id, &plans[second]);
        next_id += 1;
        plans.remove(second);
        plans[first] = merged;
    }
    plans
}

pub(crate) fn fill_cells<T: Copy + Send>(
    plan: &ClusterPlan,
    threads: usize,
    table: &mut [T],
    cell_of: impl Fn(u16, u16) -> T + Sync,
) {
    debug_assert_eq!(table.len(), plan.cells());
    let lefts = representatives(&plan.left_ids, plan.left_keys);
    let rights = representatives(&plan.right_ids, plan.right_keys);
    let chunks = Mutex::new(
        table
            .chunks_mut(ROWS_PER_CHUNK * plan.left_keys)
            .enumerate(),
    );
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let next = chunks.lock().expect("a table worker panicked").next();
                    let Some((chunk, cells)) = next else { break };
                    for (row, cells) in cells.chunks_mut(plan.left_keys).enumerate() {
                        let right = rights[chunk * ROWS_PER_CHUNK + row];
                        for (cell, &left) in cells.iter_mut().zip(&lefts) {
                            *cell = cell_of(left, right);
                        }
                    }
                }
            });
        }
    });
}

pub(crate) fn interleaved_ids(
    plans: &[ClusterPlan],
    size: usize,
    ids_of: fn(&ClusterPlan) -> &[u16],
) -> Vec<u16> {
    (0..size)
        .flat_map(|index| plans.iter().map(move |plan| ids_of(plan)[index]))
        .collect()
}

pub(crate) fn left_ids(plan: &ClusterPlan) -> &[u16] {
    &plan.left_ids
}

pub(crate) fn right_ids(plan: &ClusterPlan) -> &[u16] {
    &plan.right_ids
}

pub(crate) fn write_cluster_layout(
    output: &mut impl Write,
    shapes: impl Iterator<Item = (usize, usize)>,
    left_keys: &[u16],
    right_keys: &[u16],
    table_cells: usize,
) -> io::Result<()> {
    for (left, right) in shapes {
        for keys in [left, right] {
            output.write_all(&(keys as u32).to_le_bytes())?;
        }
    }
    write_array(output, left_keys, u16::to_le_bytes)?;
    write_array(output, right_keys, u16::to_le_bytes)?;
    output.write_all(&(table_cells as u32).to_le_bytes())?;
    output.write_all(&[0; 4])
}

fn keys_fit(keys: &[u16], limits: &[usize]) -> bool {
    keys.chunks_exact(limits.len()).all(|row| {
        row.iter()
            .zip(limits)
            .all(|(&key, &limit)| usize::from(key) < limit)
    })
}

pub(crate) struct ClusterShape {
    pub(crate) table_start: usize,
    pub(crate) left_keys: usize,
    pub(crate) right_keys: usize,
}

pub(crate) struct MappedClusters<W> {
    pub(crate) shapes: Vec<ClusterShape>,
    pub(crate) left_keys: Section<u16>,
    pub(crate) right_keys: Section<u16>,
    pub(crate) tables: Section<W>,
}

impl<W: Weight> MappedClusters<W> {
    pub(crate) fn read(
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
    pub(crate) fn sum<S: SectionSource>(
        &self,
        file: &S,
        left_right_id: u16,
        right_left_id: u16,
    ) -> W::Sum {
        let count = self.shapes.len();
        let left = &self.left_keys.view_source(file)[usize::from(left_right_id) * count..][..count];
        let right =
            &self.right_keys.view_source(file)[usize::from(right_left_id) * count..][..count];
        let tables = self.tables.view_source(file);
        let mut sum = W::Sum::default();
        for ((shape, &left), &right) in self.shapes.iter().zip(left).zip(right) {
            sum += tables
                [shape.table_start + usize::from(right) * shape.left_keys + usize::from(left)]
            .widen();
        }
        sum
    }
}
