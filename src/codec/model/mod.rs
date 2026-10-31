mod align;
mod cat;
mod digits;
mod lid;
mod num;
mod pos;
mod row;
mod spec;
mod state;
mod strings;
mod strings_group;
mod strings_plan;

use super::coder::Coder;
use state::ModelState;

pub(super) use digits::{canonical_i64, canonical_u64};
pub(crate) use spec::{FIXED_COLS, Kind, LidSpec, MAX_STR_STEPS, NumMode, Plan, Step};

pub(crate) struct Model {
    plan: Plan,
    state: ModelState,
}

impl Model {
    pub(crate) fn new(plan: Plan) -> Model {
        Model {
            state: ModelState::new(&plan),
            plan,
        }
    }

    pub(crate) fn set_active(&mut self, mask: Option<Vec<bool>>, bundle_cats: bool) {
        self.state.set_active(mask, bundle_cats);
    }

    pub(crate) fn set_srcs(&mut self, si: usize, srcs: Vec<usize>) {
        self.state.set_srcs(si, srcs);
    }

    pub(crate) fn step_bits(&self) -> &[f64] {
        &self.state.step_bits
    }

    pub(crate) fn row(&self) -> &[String] {
        self.state.row()
    }

    pub(crate) fn start_group(&mut self) {
        self.state.start_group();
    }

    #[inline]
    pub(crate) fn code_row(&mut self, coder: &mut Coder, fields: &[&str], surface: &str) {
        self.state.code_row(&self.plan, coder, fields, surface);
    }
}
