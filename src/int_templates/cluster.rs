use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::clusters::{
    ClusterPlan, ClusterShape, fill_cells, interleaved_ids, left_ids, plan_clusters, right_ids,
    write_cluster_layout,
};
use crate::part::{PartSlices, total_of};
use crate::templates::write_array;

pub(super) fn cluster_plans(slices: &[PartSlices<i32>], cluster_count: usize) -> Vec<ClusterPlan> {
    if cluster_count == 0 {
        return Vec::new();
    }
    plan_clusters(slices, cluster_count)
}

fn fill_int_table(
    plan: &ClusterPlan,
    parts: &[PartSlices<i32>],
    threads: usize,
    table: &mut [i32],
) -> io::Result<()> {
    let overflow = AtomicBool::new(false);
    fill_cells(plan, threads, table, |left, right| {
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
    Ok(())
}

pub(super) struct ClusterData {
    pub(super) shapes: Vec<ClusterShape>,
    pub(super) left_keys: Vec<u16>,
    pub(super) right_keys: Vec<u16>,
    pub(super) tables: Vec<i32>,
}

pub(super) fn cluster_data(
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
    let mut tables = vec![0i32; table_start];
    let mut unfilled = tables.as_mut_slice();
    for plan in plans {
        let (table, rest) = unfilled.split_at_mut(plan.cells());
        fill_int_table(plan, parts, threads, table)?;
        unfilled = rest;
    }
    Ok(ClusterData {
        shapes,
        left_keys: interleaved_ids(plans, left_size, left_ids),
        right_keys: interleaved_ids(plans, right_size, right_ids),
        tables,
    })
}

pub(super) fn write_clusters(data: &ClusterData, output: &mut impl Write) -> io::Result<()> {
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
