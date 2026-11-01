use super::shift::ascending_rate;
use super::{PlanBuilder, SURFACE_COL, ShiftPair, index_of_min};
use crate::codec::model::{Kind, LidSpec, Plan, Step};

const LID_ORDER_CANDIDATES: usize = 3;

impl PlanBuilder<'_> {
    fn lid_trial(&self, base: &Plan, col: usize, specs: &[LidSpec]) -> LidSpec {
        let mut probe = base.clone();
        let first = probe.steps.len();
        for sp in specs {
            probe.steps.push(Step {
                col,
                kind: Kind::Lid(sp.clone()),
            });
        }
        let active: Vec<usize> = (first..probe.steps.len()).collect();
        let bits = self.trial(&probe, &active, &[], false);
        specs[index_of_min(&bits[first..])].clone()
    }

    pub(super) fn lid_step(&self, plan: &Plan, pair: &ShiftPair) -> Step {
        let srcs: Vec<usize> = plan
            .steps
            .iter()
            .map(|s| s.col)
            .chain(std::iter::once(SURFACE_COL))
            .collect();
        let spec_with = |order: Option<usize>, key: Vec<usize>, low_col: Option<usize>| LidSpec {
            shift: pair.shift,
            partner: Some(pair.partner_col),
            order,
            key,
            low_col,
        };
        let a = pair.full_col;
        let singles: Vec<LidSpec> = srcs
            .iter()
            .map(|&c| spec_with(None, vec![c], None))
            .collect();
        let mut best = self.lid_trial(plan, a, &singles);
        let mut cand = vec![best.clone()];
        cand.extend(srcs.iter().filter(|&&c| !best.key.contains(&c)).map(|&c| {
            let mut k = best.key.clone();
            k.push(c);
            spec_with(None, k, None)
        }));
        best = self.lid_trial(plan, a, &cand);
        let mut scored: Vec<(f64, usize)> = srcs
            .iter()
            .map(|&c| (ascending_rate(self.lex, &self.sample, c, a, pair.shift), c))
            .collect();
        scored.sort_by(|x, y| y.0.total_cmp(&x.0));
        let mut cand = vec![best.clone()];
        for x in scored.iter().take(LID_ORDER_CANDIDATES) {
            cand.push(spec_with(Some(x.1), best.key.clone(), None));
        }
        best = self.lid_trial(plan, a, &cand);
        let mut cand = vec![best.clone()];
        for &c in &srcs {
            let mut sp = best.clone();
            sp.low_col = Some(c);
            cand.push(sp);
        }
        best = self.lid_trial(plan, a, &cand);
        Step {
            col: a,
            kind: Kind::Lid(best),
        }
    }
}
