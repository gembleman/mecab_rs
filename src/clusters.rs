use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Mutex;
use std::thread;

use crate::error::Error;
use crate::section::Section;
use crate::templates::{PartSlices, TemplateReader, to_cost, write_array};

const MAX_TABLE_THREADS: usize = 8;
const ROWS_PER_CHUNK: usize = 16;
const ERROR_SAFETY_FACTOR: f64 = 4.0;
const MAX_SHAPES: usize = 256;

pub(crate) struct ClusterPlan {
    id: usize,
    parts: Vec<usize>,
    left_ids: Vec<u16>,
    right_ids: Vec<u16>,
    left_keys: usize,
    right_keys: usize,
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
    dense_ids(first.iter().zip(second).map(|(&a, &b)| u32::from(a) << u16::BITS | u32::from(b)))
}

fn representatives(ids: &[u16], keys: usize) -> Vec<u16> {
    let mut first = vec![0; keys];
    for (index, &id) in ids.iter().enumerate().rev() {
        first[usize::from(id)] = index as u16;
    }
    first
}

impl ClusterPlan {
    fn of_part(id: usize, part: &PartSlices) -> Self {
        let (left_ids, left_keys) = dense_ids(part.left_projection.iter().map(|&key| u32::from(key)));
        let (right_ids, right_keys) = dense_ids(part.right_projection.iter().map(|&key| u32::from(key)));
        Self { id, parts: vec![id], left_ids, right_ids, left_keys, right_keys }
    }

    fn cells(&self) -> usize {
        self.left_keys * self.right_keys
    }

    fn merged_cells(&self, other: &Self) -> usize {
        joined_ids(&self.left_ids, &other.left_ids).1 * joined_ids(&self.right_ids, &other.right_ids).1
    }

    fn merged(&self, id: usize, other: &Self) -> Self {
        let (left_ids, left_keys) = joined_ids(&self.left_ids, &other.left_ids);
        let (right_ids, right_keys) = joined_ids(&self.right_ids, &other.right_ids);
        let mut parts = [self.parts.as_slice(), other.parts.as_slice()].concat();
        parts.sort_unstable();
        Self { id, parts, left_ids, right_ids, left_keys, right_keys }
    }
}

fn growth_of_merge(plans: &[ClusterPlan], first: usize, second: usize) -> i64 {
    let merged = plans[first].merged_cells(&plans[second]) as i64;
    merged - plans[first].cells() as i64 - plans[second].cells() as i64
}

pub(crate) fn plan_clusters(parts: &[PartSlices], target: usize) -> Vec<ClusterPlan> {
    let mut plans: Vec<ClusterPlan> =
        parts.iter().enumerate().map(|(index, part)| ClusterPlan::of_part(index, part)).collect();
    let mut next_id = plans.len();
    let mut growth: HashMap<(usize, usize), i64> = HashMap::new();
    while plans.len() > target.max(1) {
        let mut cheapest = (i64::MAX, 0, 1);
        for first in 0..plans.len() {
            for second in first + 1..plans.len() {
                let key = (plans[first].id, plans[second].id);
                let cells = *growth.entry(key).or_insert_with(|| growth_of_merge(&plans, first, second));
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

fn fill_table(plan: &ClusterPlan, parts: &[PartSlices]) -> Vec<f64> {
    let lefts = representatives(&plan.left_ids, plan.left_keys);
    let rights = representatives(&plan.right_ids, plan.right_keys);
    let mut table = vec![0.0; plan.cells()];
    let chunks = Mutex::new(table.chunks_mut(ROWS_PER_CHUNK * plan.left_keys).enumerate());
    let threads = thread::available_parallelism().map_or(1, usize::from).min(MAX_TABLE_THREADS);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let next = chunks.lock().expect("a table worker panicked").next();
                    let Some((chunk, cells)) = next else { break };
                    for (row, cells) in cells.chunks_mut(plan.left_keys).enumerate() {
                        let right = rights[chunk * ROWS_PER_CHUNK + row];
                        for (cell, &left) in cells.iter_mut().zip(&lefts) {
                            let weights = plan.parts.iter().filter_map(|&p| parts[p].weight_of(left, right));
                            *cell = weights.fold(0.0, |sum, weight| sum + weight);
                        }
                    }
                }
            });
        }
    });
    table
}

fn error_bound(parts: &[PartSlices], cluster_count: usize) -> f64 {
    let largest = |part: &PartSlices| part.weight.iter().fold(0.0_f64, |top, weight| top.max(weight.abs()));
    let magnitude: f64 = parts.iter().map(largest).sum();
    ERROR_SAFETY_FACTOR * (parts.len() + cluster_count) as f64 * magnitude * f64::EPSILON
}

pub(crate) fn write_clusters(
    parts: &[PartSlices],
    plans: &[ClusterPlan],
    left_size: usize,
    right_size: usize,
    output: &mut impl Write,
) -> io::Result<()> {
    output.write_all(&error_bound(parts, plans.len()).to_le_bytes())?;
    for plan in plans {
        for keys in [plan.left_keys, plan.right_keys] {
            output.write_all(&(keys as u32).to_le_bytes())?;
        }
    }
    for (size, ids_of) in [(left_size, left_ids as fn(&ClusterPlan) -> &[u16]), (right_size, right_ids)] {
        let interleaved: Vec<u16> =
            (0..size).flat_map(|index| plans.iter().map(move |plan| ids_of(plan)[index])).collect();
        write_array(output, &interleaved, u16::to_le_bytes)?;
    }
    let total: usize = plans.iter().map(ClusterPlan::cells).sum();
    output.write_all(&(total as u32).to_le_bytes())?;
    output.write_all(&[0; 4])?;
    for plan in plans {
        write_array(output, &fill_table(plan, parts), f64::to_le_bytes)?;
    }
    Ok(())
}

fn left_ids(plan: &ClusterPlan) -> &[u16] {
    &plan.left_ids
}

fn right_ids(plan: &ClusterPlan) -> &[u16] {
    &plan.right_ids
}

fn keys_fit(keys: &[u16], limits: &[usize]) -> bool {
    keys.chunks_exact(limits.len()).all(|row| {
        row.iter().zip(limits).all(|(&key, &limit)| usize::from(key) < limit)
    })
}

struct ClusterShape {
    table_start: usize,
    left_keys: usize,
}

pub(crate) struct MappedClusters {
    error_bound: f64,
    shapes: Vec<ClusterShape>,
    left_keys: Section<u16>,
    right_keys: Section<u16>,
    tables: Section<f64>,
}

impl MappedClusters {
    pub(crate) fn read(
        reader: &mut TemplateReader,
        count: usize,
        left_size: usize,
        right_size: usize,
    ) -> Result<Self, Error> {
        let error_bound = f64::from_le_bytes(reader.bytes()?);
        let mut shapes = Vec::with_capacity(count.min(MAX_SHAPES));
        let mut right_key_counts = Vec::with_capacity(count.min(MAX_SHAPES));
        let mut table_cells = 0usize;
        for _ in 0..count {
            let left_keys = reader.count()?;
            let right_keys = reader.count()?;
            shapes.push(ClusterShape { table_start: table_cells, left_keys });
            right_key_counts.push(right_keys);
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
        let clusters = Self { error_bound, shapes, left_keys, right_keys, tables };
        let left_limits: Vec<usize> = clusters.shapes.iter().map(|shape| shape.left_keys).collect();
        let valid = error_bound.is_finite()
            && error_bound >= 0.0
            && keys_fit(clusters.left_keys.view(reader.file), &left_limits)
            && keys_fit(clusters.right_keys.view(reader.file), &right_key_counts);
        if valid { Ok(clusters) } else { Err(reader.broken("a cluster key is out of range")) }
    }

    pub(crate) fn cost(&self, file: &[u8], cost_factor: f64, rc: u16, lc: u16) -> Option<i16> {
        let count = self.shapes.len();
        let left = &self.left_keys.view(file)[usize::from(rc) * count..][..count];
        let right = &self.right_keys.view(file)[usize::from(lc) * count..][..count];
        let tables = self.tables.view(file);
        let mut sum = 0.0;
        for ((shape, &left), &right) in self.shapes.iter().zip(left).zip(right) {
            sum += tables[shape.table_start + usize::from(right) * shape.left_keys + usize::from(left)];
        }
        let low = to_cost(sum - self.error_bound, cost_factor);
        (low == to_cost(sum + self.error_bound, cost_factor)).then_some(low)
    }
}
