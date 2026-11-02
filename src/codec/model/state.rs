use super::cat::{CatGroup, CatState, NO_PREVIOUS, code_cat, code_cat_group};
use super::digits::{canonical_i64, canonical_u64};
use super::lid::{LidState, code_lid};
use super::num::{IRREGULAR_FLAG_LIMIT, NumState, code_num};
use super::pos::PosModel;
use super::row::RowState;
use super::spec::{Kind, Plan, Step};
use super::strings::{StrState, code_str};
use super::strings_group::{StrGroup, code_str_group};
use crate::codec::coder::{Coder, Flag};
use crate::codec::hints::HintReader;
use crate::codec::ppm::PpmHint;

pub(super) enum StepState {
    Cat(Box<CatState>),
    Str(Box<StrState>),
    Lid(Box<LidState>),
    Num(Box<NumState>),
}

#[derive(Clone, Copy)]
struct Bundles {
    cat: bool,
    string: bool,
}

pub(super) struct ModelState {
    pub steps: Vec<StepState>,
    row_state: RowState,
    pos: PosModel,
    cat_group: CatGroup,
    str_group: StrGroup,
    row_flag: Flag,
    bundle_cats: bool,
    active: Option<Vec<bool>>,
    has_numbers: bool,
    pub step_bits: Vec<f64>,
}

fn build_steps(plan: &Plan, hints: &mut HintReader) -> Vec<StepState> {
    let mut steps = Vec::new();
    let mut srcs = vec![0usize];
    for s in &plan.steps {
        steps.push(match &s.kind {
            Kind::Cat => StepState::Cat(Box::new(CatState::new(hints))),
            Kind::Str { recent, .. } => {
                let st = StrState::new(*recent, srcs.clone(), hints);
                srcs.push(s.col);
                StepState::Str(Box::new(st))
            }
            Kind::Lid { .. } => StepState::Lid(Box::new(LidState::new(hints))),
            Kind::Num { .. } => StepState::Num(Box::new(NumState::new(hints))),
        });
    }
    steps
}

fn step_is_irregular(step: &Step, row: &RowState) -> bool {
    match &step.kind {
        Kind::Num { .. } => canonical_i64(&row.fields[step.col]).is_none(),
        Kind::Lid(l) => match canonical_u64(&row.fields[step.col]) {
            None => true,
            Some(v) => l
                .partner
                .is_some_and(|p| canonical_u64(&row.fields[p]) != Some(v >> l.shift)),
        },
        _ => false,
    }
}

impl ModelState {
    pub(super) fn hints(&self, out: &mut Vec<PpmHint>) {
        for step in &self.steps {
            match step {
                StepState::Cat(st) => st.hints(out),
                StepState::Str(st) => st.hints(out),
                StepState::Lid(st) => st.hints(out),
                StepState::Num(st) => st.hints(out),
            }
        }
        self.row_state.hints(out);
        self.pos.hints(out);
        self.cat_group.hints(out);
        self.str_group.hints(out);
    }

    pub(super) fn sizes(&self, out: &mut Vec<u32>) {
        for step in &self.steps {
            match step {
                StepState::Cat(st) => st.sizes(out),
                StepState::Str(st) => st.sizes(out),
                StepState::Lid(st) => st.sizes(out),
                StepState::Num(st) => st.sizes(out),
            }
        }
        self.pos.sizes(out);
        self.cat_group.sizes(out);
        self.str_group.sizes(out);
    }

    pub(super) fn new(plan: &Plan, hints: &mut HintReader) -> ModelState {
        ModelState {
            steps: build_steps(plan, hints),
            row_state: RowState::new(plan.n_cols, hints),
            pos: PosModel::new(plan.pos_cols, hints),
            cat_group: CatGroup::new(&plan.steps, plan.cat_ctx, hints),
            str_group: StrGroup::new(&plan.steps, hints),
            row_flag: Flag::default(),
            bundle_cats: true,
            active: None,
            has_numbers: plan
                .steps
                .iter()
                .any(|s| matches!(s.kind, Kind::Num { .. } | Kind::Lid(_))),
            step_bits: vec![0.0; plan.steps.len()],
        }
    }

    pub(super) fn set_active(&mut self, mask: Option<Vec<bool>>, bundle_cats: bool) {
        self.active = mask;
        self.bundle_cats = bundle_cats;
    }

    pub(super) fn set_srcs(&mut self, si: usize, srcs: Vec<usize>) {
        if let StepState::Str(st) = &mut self.steps[si] {
            st.srcs = srcs;
        }
    }

    pub(super) fn row(&self) -> &[String] {
        &self.row_state.fields
    }

    pub(super) fn start_group(&mut self) {
        self.row_state.first_in_group = true;
    }

    #[inline]
    fn code_pos(&mut self, pos_cols: usize, coder: &mut Coder) {
        if pos_cols > 0 {
            self.pos
                .code(&mut self.row_state, coder, self.active.is_some());
        }
    }

    #[inline]
    fn reset_group_state(&mut self) {
        let row = &mut self.row_state;
        row.cat_group_id = 0;
        row.str_shape = 0;
        row.str_group_id = 0;
        row.surf_hash = 0;
        row.cat_pos_ctx = None;
        row.cat_group_ctx = None;
        row.str_group_ctx = None;
        row.ref_ctx = None;
    }

    #[inline]
    fn code_irregular_flag(&mut self, plan_steps: &[Step], coder: &mut Coder) {
        let row = &mut self.row_state;
        row.row_irregular = true;
        if self.active.is_some() {
            return;
        }
        let irregular = !coder.is_decoder()
            && plan_steps
                .iter()
                .fold(false, |irr, step| irr | step_is_irregular(step, row));
        row.row_irregular =
            self.has_numbers && self.row_flag.code(coder, irregular, IRREGULAR_FLAG_LIMIT);
    }

    #[inline]
    fn note_skipped_num(&mut self, step: &Step) {
        if let Kind::Num { .. } = step.kind
            && let Some(v) = canonical_i64(&self.row_state.fields[step.col])
        {
            self.row_state.note_num(v);
        }
    }

    #[inline]
    fn code_cat_step(
        &mut self,
        plan_steps: &[Step],
        si: usize,
        col: usize,
        bundled: bool,
        coder: &mut Coder,
    ) {
        let StepState::Cat(st) = &mut self.steps[si] else {
            unreachable!()
        };
        if !bundled {
            code_cat(st, &mut self.row_state, coder, col, NO_PREVIOUS);
        } else if si == self.cat_group.steps[0] {
            code_cat_group(
                &mut self.cat_group,
                &mut self.steps,
                plan_steps,
                &mut self.row_state,
                coder,
            );
        }
    }

    #[inline]
    fn code_str_step(
        &mut self,
        plan_steps: &[Step],
        si: usize,
        step: &Step,
        bundled: bool,
        coder: &mut Coder,
    ) {
        let Kind::Str { align, .. } = step.kind else {
            unreachable!()
        };
        if bundled && si == self.str_group.steps[0] {
            code_str_group(
                &mut self.str_group,
                &mut self.steps,
                plan_steps,
                &mut self.row_state,
                coder,
            );
        }
        if let StepState::Str(st) = &mut self.steps[si] {
            code_str(st, &mut self.row_state, coder, step.col, align, bundled);
        }
    }

    #[inline]
    fn code_step(&mut self, plan_steps: &[Step], si: usize, bundles: Bundles, coder: &mut Coder) {
        let step = &plan_steps[si];
        match &step.kind {
            Kind::Cat => self.code_cat_step(plan_steps, si, step.col, bundles.cat, coder),
            Kind::Str { .. } => self.code_str_step(plan_steps, si, step, bundles.string, coder),
            Kind::Lid(spec) => {
                if let StepState::Lid(st) = &mut self.steps[si] {
                    code_lid(st, &mut self.row_state, coder, step.col, spec);
                }
            }
            Kind::Num { mode } => {
                if let StepState::Num(st) = &mut self.steps[si] {
                    code_num(st, &mut self.row_state, coder, step.col, *mode);
                }
            }
        }
    }

    #[inline]
    fn code_steps(&mut self, plan_steps: &[Step], bundles: Bundles, coder: &mut Coder) {
        for si in 0..plan_steps.len() {
            if self.active.as_ref().is_some_and(|m| !m[si]) {
                self.note_skipped_num(&plan_steps[si]);
                continue;
            }
            let before = coder.bits;
            self.code_step(plan_steps, si, bundles, coder);
            self.step_bits[si] += coder.bits - before;
        }
    }

    pub(super) fn code_row(
        &mut self,
        plan: &Plan,
        coder: &mut Coder,
        fields: &[&str],
        surface: &str,
    ) {
        self.row_state.load_fields(coder, fields, surface);
        self.code_pos(plan.pos_cols, coder);
        self.reset_group_state();
        let cat = self.bundle_cats && !self.cat_group.steps.is_empty();
        if cat {
            self.cat_group.prefetch(&mut self.row_state);
        }
        self.code_irregular_flag(&plan.steps, coder);
        let string = self.active.is_none() && !self.str_group.steps.is_empty();
        if string {
            self.str_group.prefetch(&mut self.row_state);
        }
        self.code_steps(&plan.steps, Bundles { cat, string }, coder);
        self.row_state.finish_row();
    }
}
