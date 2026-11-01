use super::{AltCost, Classified, PlanBuilder, SURFACE_COL, StrCost, cat_step, num_step};
use crate::codec::model::{Kind, NumMode, Plan, Step};

const CAT_MAX_DISTINCT: usize = 20_000;
const STR_MAX_DISTINCT_CAT: usize = 64;

enum Placement {
    Str(f64, Step),
    Cat(Step, f64),
    Num(Step),
}

impl PlanBuilder<'_> {
    pub(super) fn alternative_costs(&self, cands: &[usize]) -> Vec<AltCost> {
        let mut probe = self.probe();
        let mut kinds = Vec::new();
        for &c in cands {
            if self.small || self.info[c].distinct <= CAT_MAX_DISTINCT {
                probe.steps.push(cat_step(c));
                kinds.push((c, Kind::Cat));
            }
            if self.info[c].all_int {
                let mode = NumMode::Plain;
                probe.steps.push(num_step(c, mode));
                kinds.push((c, Kind::Num { mode }));
            }
        }
        let active: Vec<usize> = (0..probe.steps.len()).collect();
        let bits = self.trial(&probe, &active, &[], false);
        kinds
            .into_iter()
            .enumerate()
            .map(|(i, (col, kind))| AltCost {
                col,
                kind,
                bits: bits[i],
            })
            .collect()
    }

    pub(super) fn string_candidates(&self, cands: &[usize], alt_cost: &[AltCost]) -> Vec<usize> {
        cands
            .iter()
            .copied()
            .filter(|&c| {
                let has_cat = alt_cost.iter().any(|a| a.col == c && a.kind == Kind::Cat);
                self.small || self.info[c].distinct > STR_MAX_DISTINCT_CAT || !has_cat
            })
            .collect()
    }

    fn place_column(
        c: usize,
        alt_cost: &[AltCost],
        first: &[StrCost],
        full: &[StrCost],
        align_col: Option<usize>,
    ) -> Placement {
        let mut best: Option<(f64, Kind)> = None;
        for a in alt_cost {
            if a.col == c && best.as_ref().is_none_or(|b| a.bits < b.0) {
                best = Some((a.bits, a.kind.clone()));
            }
        }
        let align = Some(c) == align_col;
        let str_best = full
            .iter()
            .find(|f| f.col == c && f.align == align)
            .map(|f| (f.bits, align));
        if let Some((cost, align)) = str_best
            && best.as_ref().is_none_or(|b| cost < b.0)
        {
            let cost0 = first
                .iter()
                .filter(|f| f.col == c)
                .map(|f| f.bits)
                .fold(f64::MAX, f64::min);
            return Placement::Str(
                cost0,
                Step {
                    col: c,
                    kind: Kind::Str { align, recent: 0 },
                },
            );
        }
        match best {
            Some((_, Kind::Num { mode })) => Placement::Num(num_step(c, mode)),
            Some((cost, kind)) => Placement::Cat(Step { col: c, kind }, cost),
            None => Placement::Cat(cat_step(c), f64::MAX),
        }
    }

    pub(super) fn classify(
        cands: &[usize],
        alt_cost: &[AltCost],
        first: &[StrCost],
        full: &[StrCost],
        align_col: Option<usize>,
    ) -> Classified {
        let mut classified = Classified::default();
        for &c in cands {
            match Self::place_column(c, alt_cost, first, full, align_col) {
                Placement::Str(cost0, step) => classified.str_order.push((cost0, step)),
                Placement::Cat(step, cost) => classified.cat_steps.push((step, cost)),
                Placement::Num(step) => classified.num_steps.push(step),
            }
        }
        classified
    }

    pub(super) fn choose_cat_ctx(&self, plan: &mut Plan) {
        let cat_idx: Vec<usize> = plan
            .steps
            .iter()
            .enumerate()
            .filter(|(_, st)| st.kind == Kind::Cat)
            .map(|(i, _)| i)
            .collect();
        let mut options: Vec<Option<usize>> = vec![None, Some(SURFACE_COL)];
        options.extend(
            plan.steps
                .iter()
                .filter(|st| matches!(st.kind, Kind::Str { .. }))
                .map(|st| Some(st.col)),
        );
        let mut best_cost = f64::MAX;
        for opt in options {
            let mut probe = plan.clone();
            probe.cat_ctx = opt;
            let bits = self.trial(&probe, &cat_idx, &[], true);
            let cost: f64 = cat_idx.iter().map(|&i| bits[i]).sum();
            if cost < best_cost {
                best_cost = cost;
                plan.cat_ctx = opt;
            }
        }
    }
}
