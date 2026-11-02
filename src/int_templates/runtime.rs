use super::IntTemplates;
use super::cluster::{BuiltTables, ClusterTables, RuntimeInputs, cluster_plans, cluster_shapes};
use crate::clusters::{interleaved_ids, left_ids, right_ids};
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
        let shapes = cluster_shapes(&plans);
        let left_keys = interleaved_ids(&plans, self.left_size, left_ids);
        let right_keys = interleaved_ids(&plans, self.right_size, right_ids);
        let inputs = RuntimeInputs {
            cost_factor: self.cost_factor,
            shift: self.shift,
            left_size: self.left_size,
            right_size: self.right_size,
            shapes: &shapes,
            left_keys: &left_keys,
            right_keys: &right_keys,
            tables: ClusterTables::Built(BuiltTables {
                parts: &slices,
                plans: &plans,
                threads,
            }),
            exception_cells: &self.exception_cells,
            exception_values: &self.exception_values,
        };
        runtime_form_from(&inputs, shape)
    }
}
