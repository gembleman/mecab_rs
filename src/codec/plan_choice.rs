mod lid;
mod place;
mod sample;
mod shift;
mod strings;

use super::coder::Coder;
use super::model::{FIXED_COLS, Kind, LidSpec, MAX_STR_STEPS, Model, NumMode, Plan, Step};
use super::table::{Lexicon, Parsed};
use sample::{build_sample, col_info};
use shift::find_shift_pair;
use std::ops::Range;

type Sample = Vec<(u32, bool)>;

const SAMPLE_ROWS: usize = 30_000;
const SMALL_SAMPLE_ROWS: usize = 20_000;
const MAX_POS_COLS: usize = 6;
const SURFACE_COL: usize = 0;
const FIXED_NUM_MODES: [NumMode; FIXED_COLS - 1] = [
    NumMode::Surface,
    NumMode::SurfacePrevious,
    NumMode::LidLinked,
];

struct ShiftPair {
    full_col: usize,
    partner_col: usize,
    shift: u8,
}

struct ColInfo {
    distinct: usize,
    all_int: bool,
    all_u64: bool,
    kana_share: f64,
}

struct AltCost {
    col: usize,
    kind: Kind,
    bits: f64,
}

struct StrCost {
    col: usize,
    align: bool,
    bits: f64,
}

#[derive(Clone, Copy)]
enum StrSources {
    SurfaceOnly,
    SurfaceAndOthers,
}

#[derive(Default)]
struct Classified {
    str_order: Vec<(f64, Step)>,
    cat_steps: Vec<(Step, f64)>,
    num_steps: Vec<Step>,
}

fn cat_step(col: usize) -> Step {
    Step {
        col,
        kind: Kind::Cat,
    }
}

fn num_step(col: usize, mode: NumMode) -> Step {
    Step {
        col,
        kind: Kind::Num { mode },
    }
}

fn index_of_min(costs: &[f64]) -> usize {
    let mut best = 0;
    for i in 1..costs.len() {
        if costs[i] < costs[best] {
            best = i;
        }
    }
    best
}

fn cap_string_steps(classified: &mut Classified) -> Vec<Step> {
    let str_order = &mut classified.str_order;
    str_order.sort_by(|a, b| a.0.total_cmp(&b.0));
    for extra in str_order.drain(MAX_STR_STEPS.min(str_order.len())..) {
        classified.cat_steps.push((cat_step(extra.1.col), f64::MAX));
    }
    std::mem::take(str_order).into_iter().map(|s| s.1).collect()
}

struct PlanBuilder<'a> {
    lex: &'a Lexicon,
    sample: Sample,
    n_cols: usize,
    pos_cols: usize,
    small: bool,
    info: Vec<ColInfo>,
}

impl<'a> PlanBuilder<'a> {
    fn new(parsed: &'a Parsed, groups: &[Range<usize>]) -> Self {
        let lex = &parsed.lex;
        let n_cols = lex.n_cols;
        let pos_cols = if n_cols > FIXED_COLS {
            MAX_POS_COLS.min(n_cols - FIXED_COLS)
        } else {
            0
        };
        let sample = build_sample(parsed, groups, SAMPLE_ROWS);
        let small = sample.len() < SMALL_SAMPLE_ROWS;
        let info = (0..n_cols).map(|c| col_info(lex, &sample, c)).collect();
        PlanBuilder {
            lex,
            sample,
            n_cols,
            pos_cols,
            small,
            info,
        }
    }

    fn probe(&self) -> Plan {
        Plan::probe(self.n_cols, self.pos_cols)
    }

    fn pos_range(&self) -> Range<usize> {
        FIXED_COLS..FIXED_COLS + self.pos_cols
    }

    fn trial(
        &self,
        plan: &Plan,
        active: &[usize],
        srcs: &[(usize, Vec<usize>)],
        bundle_cats: bool,
    ) -> Vec<f64> {
        let mut m = Model::new(plan.clone());
        let mut mask = vec![false; plan.steps.len()];
        for &a in active {
            mask[a] = true;
        }
        m.set_active(Some(mask), bundle_cats);
        for (si, s) in srcs {
            m.set_srcs(*si, s.clone());
        }
        let mut coder = Coder::counter();
        let mut fields: Vec<&str> = Vec::with_capacity(self.lex.n_cols);
        for &(r, new_group) in &self.sample {
            if new_group {
                m.start_group();
            }
            fields.clear();
            for col in 0..self.lex.n_cols {
                fields.push(self.lex.field(r as usize, col));
            }
            m.code_row(&mut coder, &fields, "");
        }
        m.step_bits().to_vec()
    }

    fn fixed_numeric_steps(&self) -> Vec<Step> {
        let all_int = self.n_cols >= FIXED_COLS && (1..FIXED_COLS).all(|c| self.info[c].all_int);
        if !all_int {
            return Vec::new();
        }
        FIXED_NUM_MODES
            .iter()
            .enumerate()
            .map(|(i, &mode)| num_step(i + 1, mode))
            .collect()
    }

    fn free_cols(&self, has_fixed_numbers: bool) -> Vec<usize> {
        let mut taken = vec![false; self.n_cols];
        taken[SURFACE_COL] = true;
        for c in self.pos_range() {
            taken[c] = true;
        }
        if has_fixed_numbers {
            taken[1..FIXED_COLS].fill(true);
        }
        (1..self.n_cols).filter(|&c| !taken[c]).collect()
    }

    fn is_complete(&self, plan: &Plan) -> bool {
        let mut covered = vec![false; self.n_cols];
        covered[SURFACE_COL] = true;
        for c in self.pos_range() {
            covered[c] = true;
        }
        for st in &plan.steps {
            covered[st.col] = true;
            if let Kind::Lid(LidSpec {
                partner: Some(q), ..
            }) = &st.kind
            {
                covered[*q] = true;
            }
        }
        plan.valid() && !covered.contains(&false)
    }

    fn safe_plan(&self) -> Plan {
        let mut safe = self.probe();
        for c in 1..self.n_cols {
            if !self.pos_range().contains(&c) {
                safe.steps.push(cat_step(c));
            }
        }
        safe
    }

    fn build(&self) -> Plan {
        let num_steps = self.fixed_numeric_steps();
        let rest = self.free_cols(!num_steps.is_empty());
        let u64_cols: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|&c| self.info[c].all_u64)
            .collect();
        let lid = find_shift_pair(self.lex, &self.sample, &u64_cols);
        let mut cands = rest;
        if let Some(pair) = &lid {
            cands.retain(|&c| c != pair.full_col && c != pair.partner_col);
        }
        let alt_cost = self.alternative_costs(&cands);
        let str_cands = self.string_candidates(&cands, &alt_cost);
        let first = self.string_pass(&str_cands, StrSources::SurfaceOnly);
        let full = self.string_pass(&str_cands, StrSources::SurfaceAndOthers);
        let align_col = Self::align_column(&str_cands, &first);
        let mut classified = Self::classify(&cands, &alt_cost, &first, &full, align_col);
        let mut plan = self.assemble(&mut classified);
        if !classified.cat_steps.is_empty() {
            self.choose_cat_ctx(&mut plan);
        }
        if let Some(pair) = &lid {
            plan.steps.push(self.lid_step(&plan, pair));
        }
        plan.steps.extend(num_steps);
        if self.is_complete(&plan) {
            plan
        } else {
            self.safe_plan()
        }
    }

    fn assemble(&self, classified: &mut Classified) -> Plan {
        let mut str_steps = cap_string_steps(classified);
        self.tune_recent(&mut str_steps);
        classified.cat_steps.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut plan = self.probe();
        plan.steps.extend(str_steps);
        for s in &classified.cat_steps {
            plan.steps.push(s.0.clone());
        }
        plan.steps.append(&mut classified.num_steps);
        plan
    }
}

pub(crate) fn choose_plan(parsed: &Parsed, groups: &[Range<usize>]) -> Plan {
    if parsed.lex.n_cols == 0 {
        return Plan::probe(0, 0);
    }
    PlanBuilder::new(parsed, groups).build()
}
