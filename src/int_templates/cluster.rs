use std::borrow::Cow;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::clusters::{
    ClusterPlan, ClusterShape, fill_cells, interleaved_ids, left_ids, plan_clusters, right_ids,
    write_cluster_layout,
};
use crate::dict_files::MATRIX_ITP_FILE;
use crate::error::Error;
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
    pub(super) tables: Vec<Vec<i32>>,
}

pub(super) fn cluster_shapes(plans: &[ClusterPlan]) -> Vec<ClusterShape> {
    let mut table_start = 0;
    plans
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
        .collect()
}

pub(super) fn cluster_table(
    parts: &[PartSlices<i32>],
    plan: &ClusterPlan,
    threads: usize,
) -> io::Result<Vec<i32>> {
    let mut table = vec![0i32; plan.cells()];
    fill_int_table(plan, parts, threads, &mut table)?;
    Ok(table)
}

pub(super) fn cluster_data(
    parts: &[PartSlices<i32>],
    plans: &[ClusterPlan],
    left_size: usize,
    right_size: usize,
    threads: usize,
) -> io::Result<ClusterData> {
    let tables = plans
        .iter()
        .map(|plan| cluster_table(parts, plan, threads))
        .collect::<io::Result<_>>()?;
    Ok(ClusterData {
        shapes: cluster_shapes(plans),
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
        data.tables.iter().map(Vec::len).sum(),
    )?;
    data.tables
        .iter()
        .try_for_each(|table| write_array(output, table, i32::to_le_bytes))
}

#[cfg(feature = "xz")]
pub(crate) struct BuiltTables<'a> {
    pub(crate) parts: &'a [PartSlices<'a, i32>],
    pub(crate) plans: &'a [ClusterPlan],
    pub(crate) threads: usize,
}

pub(crate) enum ClusterTables<'a> {
    Borrowed(&'a [i32]),
    #[cfg(feature = "xz")]
    Built(BuiltTables<'a>),
}

fn borrowed_table(flat: &[i32], start: usize) -> Result<&[i32], Error> {
    flat.get(start..).ok_or_else(|| {
        Error::format(
            Path::new(MATRIX_ITP_FILE),
            "the cluster table lies outside of the matrix data",
        )
    })
}

impl ClusterTables<'_> {
    #[cfg(feature = "xz")]
    pub(crate) fn table(&self, cluster: usize, start: usize) -> Result<Cow<'_, [i32]>, Error> {
        match self {
            Self::Borrowed(flat) => borrowed_table(flat, start).map(Cow::Borrowed),
            Self::Built(built) => cluster_table(built.parts, &built.plans[cluster], built.threads)
                .map(Cow::Owned)
                .map_err(|_| Error::Unsupported("a cluster sum that does not fit an i32")),
        }
    }

    #[cfg(not(feature = "xz"))]
    pub(crate) fn table(&self, _cluster: usize, start: usize) -> Result<Cow<'_, [i32]>, Error> {
        let Self::Borrowed(flat) = self;
        borrowed_table(flat, start).map(Cow::Borrowed)
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
    pub(crate) tables: ClusterTables<'a>,
    pub(crate) exception_cells: &'a [u32],
    pub(crate) exception_values: &'a [i16],
}
