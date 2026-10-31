use super::coder::Coder;
use super::model::{
    FIXED_COLS, Kind, LidSpec, MAX_STR_STEPS, Model, NumMode, Plan, Step, canonical_i64,
    canonical_u64,
};
use super::table::{Lexicon, Parsed};
use super::text::is_kana;
use std::collections::HashSet;
use std::ops::Range;

type Sample = Vec<(u32, bool)>;

const SAMPLE_ROWS: usize = 30_000;
const SMALL_SAMPLE_ROWS: usize = 20_000;
const CAT_MAX_DISTINCT: usize = 20_000;
const STR_MAX_DISTINCT_CAT: usize = 64;
const ALIGN_MIN_KANA_SHARE: f64 = 0.3;
const MIN_ORDER_PAIRS: usize = 20;
const STR_SOURCE_LIMIT: usize = 199;
const LID_ORDER_CANDIDATES: usize = 3;
const MAX_POS_COLS: usize = 6;
const LID_SHIFT_SEARCH_END: u8 = 62;
const RECENT_SETTINGS: [u8; 4] = [0, 1, 2, 4];
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

enum Placement {
    Str(f64, Step),
    Cat(Step, f64),
    Num(Step),
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

fn build_sample(parsed: &Parsed, groups: &[Range<usize>], target: usize) -> Sample {
    let rows = parsed.order.len();
    let stride = (rows / target.max(1)).max(1);
    let mut out = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        if gi % stride != 0 {
            continue;
        }
        for (k, &r) in parsed.order[group.clone()].iter().enumerate() {
            out.push((r, k == 0));
        }
    }
    out
}

fn col_info(lex: &Lexicon, sample: &Sample, col: usize) -> ColInfo {
    let mut set: HashSet<&str> = HashSet::new();
    let (mut all_int, mut all_u64, mut kana) = (true, true, 0usize);
    for &(r, _) in sample {
        let v = lex.field(r as usize, col);
        set.insert(v);
        all_int &= canonical_i64(v).is_some();
        all_u64 &= canonical_u64(v).is_some();
        kana += usize::from(v.chars().any(is_kana));
    }
    ColInfo {
        distinct: set.len(),
        all_int: all_int && !sample.is_empty(),
        all_u64: all_u64 && !sample.is_empty(),
        kana_share: kana as f64 / sample.len().max(1) as f64,
    }
}

fn is_shift_pair(lex: &Lexicon, sample: &Sample, a: usize, b: usize, shift: u8) -> bool {
    let mut nonzero = false;
    let mut varies = false;
    let mut first = None;
    for &(r, _) in sample {
        let (Some(x), Some(y)) = (
            canonical_u64(lex.field(r as usize, a)),
            canonical_u64(lex.field(r as usize, b)),
        ) else {
            return false;
        };
        if x >> shift != y {
            return false;
        }
        nonzero |= y != 0;
        varies |= *first.get_or_insert(y) != y;
    }
    nonzero && varies
}

fn find_shift_pair(lex: &Lexicon, sample: &Sample, cols: &[usize]) -> Option<ShiftPair> {
    for &a in cols {
        for &b in cols {
            if a == b {
                continue;
            }
            if let Some(shift) =
                (1..=LID_SHIFT_SEARCH_END).find(|&s| is_shift_pair(lex, sample, a, b, s))
            {
                return Some(ShiftPair {
                    full_col: a,
                    partner_col: b,
                    shift,
                });
            }
        }
    }
    None
}

fn ascending_rate(
    lex: &Lexicon,
    sample: &Sample,
    order_col: usize,
    id_col: usize,
    shift: u8,
) -> f64 {
    let mut pairs: Vec<(&[u8], u64)> = sample
        .iter()
        .map(|&(r, _)| {
            let id = canonical_u64(lex.field(r as usize, id_col)).unwrap_or(0) >> shift;
            (lex.field_bytes(r as usize, order_col), id)
        })
        .collect();
    pairs.sort();
    pairs.dedup_by(|a, b| a.0 == b.0);
    if pairs.len() < MIN_ORDER_PAIRS {
        return 0.0;
    }
    let good = pairs.windows(2).filter(|w| w[0].1 <= w[1].1).count();
    good as f64 / (pairs.len() - 1) as f64
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

    fn alternative_costs(&self, cands: &[usize]) -> Vec<AltCost> {
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

    fn string_candidates(&self, cands: &[usize], alt_cost: &[AltCost]) -> Vec<usize> {
        cands
            .iter()
            .copied()
            .filter(|&c| {
                let has_cat = alt_cost.iter().any(|a| a.col == c && a.kind == Kind::Cat);
                self.small || self.info[c].distinct > STR_MAX_DISTINCT_CAT || !has_cat
            })
            .collect()
    }

    fn string_pass(&self, str_cands: &[usize], sources: StrSources) -> Vec<StrCost> {
        let mut probe = self.probe();
        let mut meta = Vec::new();
        let mut srcs = Vec::new();
        for &c in str_cands {
            for align in [false, true] {
                if align && self.info[c].kana_share <= ALIGN_MIN_KANA_SHARE {
                    continue;
                }
                srcs.push((probe.steps.len(), string_sources(sources, str_cands, c)));
                probe.steps.push(Step {
                    col: c,
                    kind: Kind::Str { align, recent: 0 },
                });
                meta.push((c, align));
            }
        }
        let active: Vec<usize> = (0..probe.steps.len()).collect();
        let bits = self.trial(&probe, &active, &srcs, false);
        meta.into_iter()
            .enumerate()
            .map(|(i, (col, align))| StrCost {
                col,
                align,
                bits: bits[i],
            })
            .collect()
    }

    fn align_column(str_cands: &[usize], first: &[StrCost]) -> Option<usize> {
        let mut align_col = None;
        let mut best_gain = 0.0;
        for &c in str_cands {
            let cost_of = |a: bool| {
                first
                    .iter()
                    .find(|f| f.col == c && f.align == a)
                    .map(|f| f.bits)
            };
            if let (Some(t), Some(f)) = (cost_of(true), cost_of(false))
                && f - t > best_gain
            {
                best_gain = f - t;
                align_col = Some(c);
            }
        }
        align_col
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

    fn classify(
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

    fn tune_recent(&self, str_steps: &mut [Step]) {
        let mut probe = self.probe();
        let mut srcs = Vec::new();
        let mut srcs_so_far = vec![SURFACE_COL];
        for st in str_steps.iter() {
            for &r in &RECENT_SETTINGS {
                let mut kind = st.kind.clone();
                if let Kind::Str { recent, .. } = &mut kind {
                    *recent = r;
                }
                srcs.push((probe.steps.len(), srcs_so_far.clone()));
                probe.steps.push(Step { col: st.col, kind });
            }
            srcs_so_far.push(st.col);
        }
        let active: Vec<usize> = (0..probe.steps.len()).collect();
        let bits = self.trial(&probe, &active, &srcs, false);
        for (i, st) in str_steps.iter_mut().enumerate() {
            let per_step = &bits[i * RECENT_SETTINGS.len()..(i + 1) * RECENT_SETTINGS.len()];
            let best = index_of_min(per_step);
            if let Kind::Str { recent, .. } = &mut st.kind {
                *recent = RECENT_SETTINGS[best];
            }
        }
    }

    fn choose_cat_ctx(&self, plan: &mut Plan) {
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

    fn lid_step(&self, plan: &Plan, pair: &ShiftPair) -> Step {
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

fn string_sources(sources: StrSources, str_cands: &[usize], col: usize) -> Vec<usize> {
    let mut v = vec![SURFACE_COL];
    if let StrSources::SurfaceAndOthers = sources {
        v.extend(
            str_cands
                .iter()
                .copied()
                .filter(|&x| x != col)
                .take(STR_SOURCE_LIMIT),
        );
    }
    v
}

fn cap_string_steps(classified: &mut Classified) -> Vec<Step> {
    let str_order = &mut classified.str_order;
    str_order.sort_by(|a, b| a.0.total_cmp(&b.0));
    for extra in str_order.drain(MAX_STR_STEPS.min(str_order.len())..) {
        classified.cat_steps.push((cat_step(extra.1.col), f64::MAX));
    }
    std::mem::take(str_order).into_iter().map(|s| s.1).collect()
}

pub(crate) fn choose_plan(parsed: &Parsed, groups: &[Range<usize>]) -> Plan {
    if parsed.lex.n_cols == 0 {
        return Plan::probe(0, 0);
    }
    PlanBuilder::new(parsed, groups).build()
}
