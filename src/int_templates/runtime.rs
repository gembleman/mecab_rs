use std::borrow::Cow;

use super::IntTemplates;
use super::cluster::{ClusterData, RuntimeInputs, cluster_data, cluster_plans};
use crate::error::Error;
use crate::hot_shape::HotShape;
use crate::int_runtime::runtime_form_from;
use crate::part::{IntPart, PartSlices};

impl IntTemplates {
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
        let ClusterData {
            shapes,
            left_keys,
            right_keys,
            tables,
        } = data;
        let inputs = RuntimeInputs {
            cost_factor: self.cost_factor,
            shift: self.shift,
            left_size: self.left_size,
            right_size: self.right_size,
            shapes: &shapes,
            left_keys: &left_keys,
            right_keys: &right_keys,
            tables: Cow::Owned(tables),
            exception_cells: &self.exception_cells,
            exception_values: &self.exception_values,
        };
        runtime_form_from(inputs, shape)
    }
}
