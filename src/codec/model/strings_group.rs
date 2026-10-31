use super::row::RowState;
use super::spec::{Kind, Step};
use super::state::StepState;
use super::strings::{GroupPlan, StrState};
use super::strings_plan::plan_str;
use crate::codec::coder::Coder;
use crate::codec::ppm::{Interner, NOVEL, Ppm, mix};

const EX_KEY_BYTES: usize = 2;

pub(super) struct StrGroup {
    ppm: Ppm,
    vecs: Interner,
    pub steps: Vec<usize>,
}

fn push_ex_key(key: &mut Vec<u8>, ex: u32) {
    key.extend_from_slice(&(ex as u16).to_le_bytes());
}

fn ex_at(key: &[u8], k: usize) -> u32 {
    key.get(EX_KEY_BYTES * k..EX_KEY_BYTES * (k + 1))
        .map_or(0, |x| u32::from(u16::from_le_bytes([x[0], x[1]])))
}

impl StrGroup {
    pub(super) fn new(plan_steps: &[Step]) -> StrGroup {
        StrGroup {
            ppm: Ppm::default(),
            vecs: Interner::default(),
            steps: plan_steps
                .iter()
                .enumerate()
                .filter(|(_, s)| matches!(s.kind, Kind::Str { .. }))
                .map(|(i, _)| i)
                .collect(),
        }
    }

    #[inline]
    pub(super) fn prefetch(&self, row: &RowState) {
        self.ppm.prefetch(mix(
            mix(66 + u64::from(row.first_in_group), row.pos_hash),
            u64::from(row.prev_str_group_id),
        ));
    }

    fn plan_members(&self, steps: &mut [StepState], plan_steps: &[Step], row: &mut RowState) {
        row.key_buf.clear();
        for &si in &self.steps {
            if let (StepState::Str(st), Kind::Str { align, .. }) =
                (&mut steps[si], &plan_steps[si].kind)
            {
                let (ex, choice) = plan_str(st, row, plan_steps[si].col, *align);
                st.group_plan = Some(GroupPlan { exact: ex, choice });
                push_ex_key(&mut row.key_buf, ex);
            }
        }
    }

    fn code_members(
        &mut self,
        steps: &mut [StepState],
        row: &mut RowState,
        coder: &mut Coder,
    ) -> u32 {
        row.key_buf.clear();
        for &si in &self.steps {
            if let StepState::Str(st) = &mut steps[si] {
                let want = st.group_plan.map_or(0, |p| p.exact);
                let ex = st.code_ex(row, coder, want);
                st.group_plan = st.group_plan.map(|p| GroupPlan { exact: ex, ..p });
                st.group_exact = ex;
                push_ex_key(&mut row.key_buf, ex);
            }
        }
        self.vecs.find_or_add(&row.key_buf)
    }

    fn load_members(&self, steps: &mut [StepState], row: &RowState, id: u32) {
        let key = self.vecs.get(id);
        for (k, &si) in self.steps.iter().enumerate() {
            if let StepState::Str(st) = &mut steps[si] {
                st.group_exact = ex_at(key, k);
                if st.group_exact == 0 {
                    prefetch_tuple(st, row);
                }
            }
        }
    }
}

#[inline]
fn prefetch_tuple(st: &StrState, row: &RowState) {
    st.tuple_ppm
        .prefetch(mix(mix(24, row.pos_hash), u64::from(row.str_group_id)));
    st.tuple_ppm.prefetch(mix(20, row.pos_hash));
}

pub(super) fn code_str_group(
    g: &mut StrGroup,
    steps: &mut [StepState],
    plan_steps: &[Step],
    row: &mut RowState,
    coder: &mut Coder,
) {
    let dec = coder.is_decoder();
    if dec {
        row.key_buf.clear();
    } else {
        g.plan_members(steps, plan_steps, row);
    }
    let sym = if dec {
        NOVEL
    } else {
        g.vecs.find(&row.key_buf).unwrap_or(NOVEL)
    };
    let ctxs = [
        mix(
            mix(66 + u64::from(row.first_in_group), row.pos_hash),
            u64::from(row.prev_str_group_id),
        ),
        mix(60, row.pos_hash),
        mix(61, row.pos_first_hash),
        62,
    ];
    let id = if let Some(id) = g.ppm.code(coder, &ctxs, sym) {
        id
    } else {
        g.code_members(steps, row, coder)
    };
    g.ppm.update(id);
    row.str_group_id = id + 1;
    g.load_members(steps, row, id);
}
