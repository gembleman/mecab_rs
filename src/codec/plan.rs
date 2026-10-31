use super::coder::Coder;
use super::model::{
    FIXED_COLS, Kind, LidSpec, MAX_STR_STEPS, Model, Plan, Step, canonical_i64, canonical_u64,
};
use super::table::{Lexicon, Parsed};
use super::text::is_kana;
use std::collections::HashSet;

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

fn build_sample(parsed: &Parsed, groups: &[(usize, usize)], target: usize) -> Sample {
    let rows = parsed.order.len();
    let stride = (rows / target.max(1)).max(1);
    let mut out = Vec::new();
    for (gi, &(a, b)) in groups.iter().enumerate() {
        if gi % stride != 0 {
            continue;
        }
        for (k, &r) in parsed.order[a..b].iter().enumerate() {
            out.push((r, k == 0));
        }
    }
    out
}

fn trial(
    lex: &Lexicon,
    sample: &Sample,
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
    let mut fields: Vec<&str> = Vec::with_capacity(lex.n_cols);
    for &(r, new_group) in sample {
        if new_group {
            m.start_group();
        }
        fields.clear();
        for col in 0..lex.n_cols {
            fields.push(lex.field(r as usize, col));
        }
        m.code_row(&mut coder, &fields, "");
    }
    m.step_bits.clone()
}

struct ColInfo {
    distinct: usize,
    all_int: bool,
    all_u64: bool,
    kana_share: f64,
}

fn col_info(lex: &Lexicon, sample: &Sample, col: usize) -> ColInfo {
    let mut set: HashSet<&str> = HashSet::new();
    let (mut all_int, mut all_u64, mut kana) = (true, true, 0usize);
    for &(r, _) in sample {
        let v = lex.field(r as usize, col);
        set.insert(v);
        all_int &= canonical_i64(v).is_some();
        all_u64 &= canonical_u64(v).is_some();
        kana += v.chars().any(is_kana) as usize;
    }
    ColInfo {
        distinct: set.len(),
        all_int: all_int && !sample.is_empty(),
        all_u64: all_u64 && !sample.is_empty(),
        kana_share: kana as f64 / sample.len().max(1) as f64,
    }
}

fn find_shift_pair(lex: &Lexicon, sample: &Sample, cols: &[usize]) -> Option<(usize, usize, u8)> {
    for &a in cols {
        for &b in cols {
            if a == b {
                continue;
            }
            'shift: for s in 1..=LID_SHIFT_SEARCH_END {
                let mut nonzero = false;
                let mut varies = false;
                let mut first = None;
                for &(r, _) in sample {
                    let (Some(x), Some(y)) = (
                        canonical_u64(lex.field(r as usize, a)),
                        canonical_u64(lex.field(r as usize, b)),
                    ) else {
                        continue 'shift;
                    };
                    if x >> s != y {
                        continue 'shift;
                    }
                    nonzero |= y != 0;
                    varies |= *first.get_or_insert(y) != y;
                }
                if nonzero && varies {
                    return Some((a, b, s));
                }
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

fn lid_trial_costs(
    lex: &Lexicon,
    sample: &Sample,
    base: &Plan,
    col: usize,
    specs: &[LidSpec],
) -> Vec<f64> {
    let mut probe = base.clone();
    let first = probe.steps.len();
    for sp in specs {
        probe.steps.push(Step {
            col,
            kind: Kind::Lid(sp.clone()),
        });
    }
    let active: Vec<usize> = (first..probe.steps.len()).collect();
    let bits = trial(lex, sample, &probe, &active, &[], false);
    bits[first..].to_vec()
}

fn lid_trial(
    lex: &Lexicon,
    sample: &Sample,
    base: &Plan,
    col: usize,
    specs: &[LidSpec],
) -> LidSpec {
    let costs = lid_trial_costs(lex, sample, base, col, specs);
    let mut best = 0;
    for i in 1..specs.len() {
        if costs[i] < costs[best] {
            best = i;
        }
    }
    specs[best].clone()
}

pub fn choose_plan(parsed: &Parsed, groups: &[(usize, usize)]) -> Plan {
    let lex = &parsed.lex;
    let n = lex.n_cols;
    let mut plan = Plan {
        n_cols: n,
        ..Default::default()
    };
    if n == 0 {
        return plan;
    }
    let p = if n > FIXED_COLS {
        MAX_POS_COLS.min(n - FIXED_COLS)
    } else {
        0
    };
    plan.pos_cols = p;
    let sample = build_sample(parsed, groups, SAMPLE_ROWS);
    let small = sample.len() < SMALL_SAMPLE_ROWS;
    let pos_range = FIXED_COLS..FIXED_COLS + p;
    let info: Vec<ColInfo> = (0..n).map(|c| col_info(lex, &sample, c)).collect();

    let mut num_steps = Vec::new();
    let mut taken = vec![false; n];
    taken[0] = true;
    for c in pos_range.clone() {
        taken[c] = true;
    }
    if n >= FIXED_COLS && (1..FIXED_COLS).all(|c| info[c].all_int) {
        num_steps.push(Step {
            col: 1,
            kind: Kind::Num { mode: 1 },
        });
        num_steps.push(Step {
            col: 2,
            kind: Kind::Num { mode: 3 },
        });
        num_steps.push(Step {
            col: 3,
            kind: Kind::Num { mode: 2 },
        });
        taken[1..FIXED_COLS].fill(true);
    }
    let rest: Vec<usize> = (1..n).filter(|&c| !taken[c]).collect();

    let u64_cols: Vec<usize> = rest.iter().copied().filter(|&c| info[c].all_u64).collect();
    let lid = find_shift_pair(lex, &sample, &u64_cols);
    let mut cands: Vec<usize> = rest.clone();
    if let Some((a, b, _)) = lid {
        cands.retain(|&c| c != a && c != b);
    }

    let mut alt_cost: Vec<(usize, Kind, f64)> = Vec::new();
    {
        let mut probe = Plan {
            n_cols: n,
            pos_cols: p,
            ..Default::default()
        };
        let mut kinds = Vec::new();
        for &c in &cands {
            if small || info[c].distinct <= CAT_MAX_DISTINCT {
                probe.steps.push(Step {
                    col: c,
                    kind: Kind::Cat,
                });
                kinds.push((c, Kind::Cat));
            }
            if info[c].all_int {
                probe.steps.push(Step {
                    col: c,
                    kind: Kind::Num { mode: 0 },
                });
                kinds.push((c, Kind::Num { mode: 0 }));
            }
        }
        let active: Vec<usize> = (0..probe.steps.len()).collect();
        let bits = trial(lex, &sample, &probe, &active, &[], false);
        for (i, (c, k)) in kinds.into_iter().enumerate() {
            alt_cost.push((c, k, bits[i]));
        }
    }

    let mut str_cands: Vec<usize> = Vec::new();
    for &c in &cands {
        let has_cat = alt_cost.iter().any(|a| a.0 == c && a.1 == Kind::Cat);
        if small || info[c].distinct > STR_MAX_DISTINCT_CAT || !has_cat {
            str_cands.push(c);
        }
    }
    let str_pass = |srcs_of: &dyn Fn(usize) -> Vec<usize>| -> Vec<(usize, bool, f64)> {
        let mut probe = Plan {
            n_cols: n,
            pos_cols: p,
            ..Default::default()
        };
        let mut meta = Vec::new();
        let mut srcs = Vec::new();
        for &c in &str_cands {
            for align in [false, true] {
                if align && info[c].kana_share <= ALIGN_MIN_KANA_SHARE {
                    continue;
                }
                srcs.push((probe.steps.len(), srcs_of(c)));
                probe.steps.push(Step {
                    col: c,
                    kind: Kind::Str { align, recent: 0 },
                });
                meta.push((c, align));
            }
        }
        let active: Vec<usize> = (0..probe.steps.len()).collect();
        let bits = trial(lex, &sample, &probe, &active, &srcs, false);
        meta.into_iter()
            .enumerate()
            .map(|(i, (c, a))| (c, a, bits[i]))
            .collect()
    };
    let first = str_pass(&|_| vec![0]);
    let all_cols: Vec<usize> = str_cands.clone();
    let full = str_pass(&|c| {
        let mut v = vec![0];
        v.extend(
            all_cols
                .iter()
                .copied()
                .filter(|&x| x != c)
                .take(STR_SOURCE_LIMIT),
        );
        v
    });
    let mut align_col = None;
    let mut best_gain = 0.0;
    for &c in &str_cands {
        let cost_of = |a: bool| first.iter().find(|f| f.0 == c && f.1 == a).map(|f| f.2);
        if let (Some(t), Some(f)) = (cost_of(true), cost_of(false))
            && f - t > best_gain
        {
            best_gain = f - t;
            align_col = Some(c);
        }
    }
    let mut str_order: Vec<(f64, Step)> = Vec::new();
    let mut cat_steps: Vec<(Step, f64)> = Vec::new();
    let mut num0_steps: Vec<Step> = Vec::new();
    for &c in &cands {
        let mut best: Option<(f64, Kind)> = None;
        for (cc, k, cost) in &alt_cost {
            if *cc == c && best.as_ref().is_none_or(|b| *cost < b.0) {
                best = Some((*cost, k.clone()));
            }
        }
        let align = Some(c) == align_col;
        let str_best = full
            .iter()
            .find(|f| f.0 == c && f.1 == align)
            .map(|f| (f.2, align));
        if let Some((cost, align)) = str_best
            && best.as_ref().is_none_or(|b| cost < b.0)
        {
            let cost0 = first
                .iter()
                .filter(|f| f.0 == c)
                .map(|f| f.2)
                .fold(f64::MAX, f64::min);
            str_order.push((
                cost0,
                Step {
                    col: c,
                    kind: Kind::Str { align, recent: 0 },
                },
            ));
            continue;
        }
        match best {
            Some((_, Kind::Num { mode })) => num0_steps.push(Step {
                col: c,
                kind: Kind::Num { mode },
            }),
            Some((cost, kind)) => cat_steps.push((Step { col: c, kind }, cost)),
            None => cat_steps.push((
                Step {
                    col: c,
                    kind: Kind::Cat,
                },
                f64::MAX,
            )),
        }
    }
    str_order.sort_by(|a, b| a.0.total_cmp(&b.0));
    for extra in str_order.drain(MAX_STR_STEPS.min(str_order.len())..) {
        cat_steps.push((
            Step {
                col: extra.1.col,
                kind: Kind::Cat,
            },
            f64::MAX,
        ));
    }
    let mut str_steps: Vec<Step> = str_order.into_iter().map(|s| s.1).collect();
    {
        let settings = [0u8, 1, 2, 4];
        let mut probe = Plan {
            n_cols: n,
            pos_cols: p,
            ..Default::default()
        };
        let mut srcs = Vec::new();
        let mut srcs_so_far = vec![0usize];
        for st in &str_steps {
            for &r in &settings {
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
        let bits = trial(lex, &sample, &probe, &active, &srcs, false);
        for (i, st) in str_steps.iter_mut().enumerate() {
            let mut best = 0;
            for k in 1..settings.len() {
                if bits[i * settings.len() + k] < bits[i * settings.len() + best] {
                    best = k;
                }
            }
            if let Kind::Str { recent, .. } = &mut st.kind {
                *recent = settings[best];
            }
        }
    }

    cat_steps.sort_by(|a, b| a.1.total_cmp(&b.1));
    plan.steps.extend(str_steps.iter().cloned());
    for s in &cat_steps {
        plan.steps.push(s.0.clone());
    }
    plan.steps.extend(num0_steps);

    if !cat_steps.is_empty() {
        let cat_idx: Vec<usize> = plan
            .steps
            .iter()
            .enumerate()
            .filter(|(_, st)| st.kind == Kind::Cat)
            .map(|(i, _)| i)
            .collect();
        let mut options: Vec<Option<usize>> = vec![None, Some(0)];
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
            let bits = trial(lex, &sample, &probe, &cat_idx, &[], true);
            let cost: f64 = cat_idx.iter().map(|&i| bits[i]).sum();
            if cost < best_cost {
                best_cost = cost;
                plan.cat_ctx = opt;
            }
        }
    }

    if let Some((a, b, shift)) = lid {
        let srcs: Vec<usize> = plan
            .steps
            .iter()
            .map(|s| s.col)
            .chain(std::iter::once(0))
            .collect();
        let mut scored: Vec<(f64, usize)> = srcs
            .iter()
            .map(|&c| (ascending_rate(lex, &sample, c, a, shift), c))
            .collect();
        scored.sort_by(|x, y| y.0.total_cmp(&x.0));
        let spec_with = |order: Option<usize>, key: Vec<usize>, low_col: Option<usize>| LidSpec {
            shift,
            partner: Some(b),
            order,
            key,
            low_col,
        };
        let specs: Vec<LidSpec> = srcs
            .iter()
            .map(|&c| spec_with(None, vec![c], None))
            .collect();
        let mut best = lid_trial(lex, &sample, &plan, a, &specs);
        let specs: Vec<LidSpec> = srcs
            .iter()
            .filter(|&&c| !best.key.contains(&c))
            .map(|&c| {
                let mut k = best.key.clone();
                k.push(c);
                spec_with(None, k, None)
            })
            .collect();
        let mut cand = vec![best.clone()];
        cand.extend(specs);
        best = lid_trial(lex, &sample, &plan, a, &cand);
        let mut cand = vec![best.clone()];
        for x in scored.iter().take(LID_ORDER_CANDIDATES) {
            cand.push(spec_with(Some(x.1), best.key.clone(), None));
        }
        best = lid_trial(lex, &sample, &plan, a, &cand);
        let mut cand = vec![best.clone()];
        for &c in &srcs {
            let mut sp = best.clone();
            sp.low_col = Some(c);
            cand.push(sp);
        }
        best = lid_trial(lex, &sample, &plan, a, &cand);
        plan.steps.push(Step {
            col: a,
            kind: Kind::Lid(best),
        });
    }
    plan.steps.extend(num_steps);
    let mut covered = vec![false; n];
    covered[0] = true;
    for flag in covered.iter_mut().take(FIXED_COLS + p).skip(FIXED_COLS) {
        *flag = true;
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
    if !plan.valid() || covered.contains(&false) {
        let mut safe = Plan {
            n_cols: n,
            pos_cols: p,
            ..Default::default()
        };
        for c in 1..n {
            if !(FIXED_COLS..FIXED_COLS + p).contains(&c) {
                safe.steps.push(Step {
                    col: c,
                    kind: Kind::Cat,
                });
            }
        }
        return safe;
    }
    plan
}
