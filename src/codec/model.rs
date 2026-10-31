use super::coder::{Coder, Flag, UintModel};
use super::ppm::{FastMap, Interner, NOVEL, Ppm, hash_bytes, mix};
use super::text::{CharModel, is_kana, kata};
use crate::wire::{get_varint, put_varint, unzigzag, zigzag};
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;

const TAG_CAT: u8 = 0;
const TAG_STR: u8 = 1;
const TAG_STR_ALIGNED: u8 = 2;
const TAG_NUM: u8 = 3;
const TAG_LID: u8 = 4;
const MAX_COLS: usize = 1 << 20;
const CANONICAL_I64_LIMIT: u64 = 1 << 61;
const CANONICAL_U64_LIMIT: u64 = 1 << 62;
pub const LID_MODE_COUNT: usize = 5;
const KNOWN_SEGMENT_COST: f64 = 2.0;
const NEW_SEGMENT_BASE_COST: f64 = 5.0;
const NEW_SEGMENT_COST_PER_CHAR: f64 = 6.0;
const MAX_SEGMENT_LEN: usize = 8;
const EX_ALIGNED: u32 = 1000;
const EX_DOMAIN: u64 = 1001;
const MAX_RECENT: usize = 8;
pub const FIXED_COLS: usize = 4;
const IRREGULAR_FLAG_LIMIT: u32 = 30000;
const LO_BITS_MAX: u8 = 24;
pub const MAX_STR_STEPS: usize = 200;
pub const MAX_LID_SHIFT: u8 = 54;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LidSpec {
    pub shift: u8,
    pub partner: Option<usize>,
    pub order: Option<usize>,
    pub key: Vec<usize>,
    pub low_col: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Cat,
    Str { align: bool, recent: u8 },
    Lid(LidSpec),
    Num { mode: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub col: usize,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Plan {
    pub n_cols: usize,
    pub pos_cols: usize,
    pub cat_ctx: Option<usize>,
    pub steps: Vec<Step>,
}

fn put_opt(out: &mut Vec<u8>, v: Option<usize>) {
    put_varint(out, v.map_or(0, |x| x as u64 + 1));
}

#[allow(
    clippy::option_option,
    reason = "outer None is truncated input, inner None is an absent column"
)]
fn get_opt(bytes: &[u8], pos: &mut usize) -> Option<Option<usize>> {
    let v = get_varint(bytes, pos)?;
    Some(if v == 0 { None } else { Some(v as usize - 1) })
}

fn read_u8(bytes: &[u8], pos: &mut usize) -> Option<u8> {
    let value = *bytes.get(*pos)?;
    *pos += 1;
    Some(value)
}

fn read_usize(bytes: &[u8], pos: &mut usize) -> Option<usize> {
    Some(get_varint(bytes, pos)? as usize)
}

impl Plan {
    pub fn write(&self, out: &mut Vec<u8>) {
        put_varint(out, self.n_cols as u64);
        put_varint(out, self.pos_cols as u64);
        put_opt(out, self.cat_ctx);
        put_varint(out, self.steps.len() as u64);
        for step in &self.steps {
            put_varint(out, step.col as u64);
            match &step.kind {
                Kind::Cat => out.push(TAG_CAT),
                Kind::Str { align, recent } => {
                    out.push(TAG_STR + u8::from(*align));
                    out.push(*recent);
                }
                Kind::Num { mode } => {
                    out.push(TAG_NUM);
                    out.push(*mode);
                }
                Kind::Lid(spec) => {
                    out.push(TAG_LID);
                    out.push(spec.shift);
                    put_opt(out, spec.partner);
                    put_opt(out, spec.order);
                    put_opt(out, spec.low_col);
                    put_varint(out, spec.key.len() as u64);
                    for &key_col in &spec.key {
                        put_varint(out, key_col as u64);
                    }
                }
            }
        }
    }

    pub fn read(bytes: &[u8], pos: &mut usize) -> Option<Plan> {
        let n_cols = read_usize(bytes, pos)?;
        let pos_cols = read_usize(bytes, pos)?;
        let cat_ctx = get_opt(bytes, pos)?;
        let step_count = read_usize(bytes, pos)?;
        if n_cols > MAX_COLS || step_count > n_cols || pos_cols > n_cols.saturating_sub(FIXED_COLS)
        {
            return None;
        }
        let mut steps = Vec::new();
        for _ in 0..step_count {
            let col = read_usize(bytes, pos)?;
            let tag = read_u8(bytes, pos)?;
            let kind = match tag {
                TAG_CAT => Kind::Cat,
                TAG_STR | TAG_STR_ALIGNED => {
                    let recent = read_u8(bytes, pos)?;
                    if recent as usize > MAX_RECENT {
                        return None;
                    }
                    Kind::Str {
                        align: tag == TAG_STR_ALIGNED,
                        recent,
                    }
                }
                TAG_NUM => {
                    let mode = read_u8(bytes, pos)?;
                    Kind::Num { mode }
                }
                TAG_LID => {
                    let shift = read_u8(bytes, pos)?;
                    let partner = get_opt(bytes, pos)?;
                    let order = get_opt(bytes, pos)?;
                    let low_col = get_opt(bytes, pos)?;
                    let key_len = read_usize(bytes, pos)?;
                    if key_len > n_cols {
                        return None;
                    }
                    let mut key = Vec::new();
                    for _ in 0..key_len {
                        key.push(read_usize(bytes, pos)?);
                    }
                    Kind::Lid(LidSpec {
                        shift,
                        partner,
                        order,
                        key,
                        low_col,
                    })
                }
                _ => return None,
            };
            if col >= n_cols {
                return None;
            }
            steps.push(Step { col, kind });
        }
        let plan = Plan {
            n_cols,
            pos_cols,
            cat_ctx,
            steps,
        };
        plan.valid().then_some(plan)
    }

    pub fn valid(&self) -> bool {
        let mut coded = vec![false; self.n_cols];
        if self.n_cols > 0 {
            coded[0] = true;
        }
        for flag in coded
            .iter_mut()
            .take(FIXED_COLS + self.pos_cols)
            .skip(FIXED_COLS)
        {
            *flag = true;
        }
        let mut strings = 0;
        for s in &self.steps {
            let refs: Vec<usize> = match &s.kind {
                Kind::Lid(l) => {
                    if l.shift == 0 || l.shift > MAX_LID_SHIFT {
                        return false;
                    }
                    l.order
                        .iter()
                        .chain(l.key.iter())
                        .chain(l.low_col.iter())
                        .copied()
                        .collect()
                }
                Kind::Str { align: true, .. } if s.col == 0 => return false,
                Kind::Cat => self.cat_ctx.into_iter().collect(),
                _ => Vec::new(),
            };
            if refs.iter().any(|&r| r >= self.n_cols || !coded[r]) || coded[s.col] {
                return false;
            }
            if matches!(s.kind, Kind::Str { .. }) {
                strings += 1;
                if strings > MAX_STR_STEPS {
                    return false;
                }
            }
            coded[s.col] = true;
            if let Kind::Lid(LidSpec {
                partner: Some(p), ..
            }) = &s.kind
            {
                if *p >= self.n_cols || coded[*p] {
                    return false;
                }
                coded[*p] = true;
            }
        }
        true
    }
}

struct CatState {
    ppm: Ppm,
    intern: Interner,
}

struct CatGroup {
    ppm: Ppm,
    tuples: Interner,
    steps: Vec<usize>,
    ctx_col: Option<usize>,
}

struct StrState {
    srcs: Vec<usize>,
    tuple_ppm: Ppm,
    tuples: Interner,
    ref_ppm: Ppm,
    trim: UintModel,
    chars: CharModel,
    ex_ppm: Ppm,
    run_ppm: Ppm,
    run_readings: Interner,
    read_chars: CharModel,
    recent: Vec<String>,
    limit: usize,
    group_plan: Option<(u32, Cand)>,
    group_exact: u32,
    align_stats: [f64; 4],
    seg_ppm: Ppm,
    segs: Interner,
    known: HashSet<u64>,
}

struct LidState {
    seen: FastMap<u64, u64>,
    order: BTreeMap<Vec<u8>, (u64, u64)>,
    mode_ppm: Ppm,
    delta: UintModel,
    direct: UintModel,
    from_last: UintModel,
    lo_ppm: Ppm,
    hi_ppm: Ppm,
    irregular: Flag,
    partner_bad: Flag,
    last_new: u64,
    stats: [f64; 6],
    mode_count: [u64; LID_MODE_COUNT],
}

struct NumState {
    ppm: Ppm,
    intern: Interner,
    uint: UintModel,
    irregular: Flag,
    d_ppm: Ppm,
    d_intern: Interner,
    d_uint: UintModel,
    refs: FastMap<u64, (i64, i64)>,
    pub novel: (f64, u64),
}

enum StepState {
    Cat(Box<CatState>),
    Str(Box<StrState>),
    Lid(Box<LidState>),
    Num(Box<NumState>),
}

struct RowState {
    fields: Vec<String>,
    coded_text: String,
    key_buf: Vec<u8>,
    raw_chars: CharModel,
    cat_chars: CharModel,
    pos_hash: u64,
    pos_first_hash: u64,
    pos_first_two_hash: u64,
    pos_sym: u32,
    prev_pos: u32,
    last_char: u32,
    first_in_group: bool,
    prev_num: i64,
    num_seen: bool,
    lid_ref_key: u64,
    cat_group_id: u32,
    surf_hash: u64,
    str_shape: u64,
    str_group_id: u32,
    prev_str_group_id: u32,
    prev_cat_group_id: u32,
    row_irregular: bool,
    rebuilt: String,
    script_runs: Vec<(bool, usize, usize)>,
    reading_spans: Vec<(usize, usize)>,
}

pub struct Model {
    plan: Plan,
    steps: Vec<StepState>,
    row_state: RowState,
    pos_ppm: Ppm,
    pos_intern: Interner,
    comp_ppm: Vec<Ppm>,
    comp_intern: Vec<Interner>,
    comp_chars: CharModel,
    pos_hashes: Vec<(u64, u64, u64)>,
    cat_group: CatGroup,
    str_group: StrGroup,
    row_flag: Flag,
    bundle_cats: bool,
    active: Option<Vec<bool>>,
    pub step_bits: Vec<f64>,
    pub pos_bits: f64,
}

pub(super) fn canonical_i64(s: &str) -> Option<i64> {
    let v: i64 = s.parse().ok()?;
    if v.unsigned_abs() < CANONICAL_I64_LIMIT && v.to_string() == s {
        Some(v)
    } else {
        None
    }
}

pub(super) fn canonical_u64(s: &str) -> Option<u64> {
    let v: u64 = s.parse().ok()?;
    if v < CANONICAL_U64_LIMIT && v.to_string() == s {
        Some(v)
    } else {
        None
    }
}

#[inline]
fn read_str(bytes: &[u8], pos: &mut usize, dest: &mut String) {
    let len = get_varint(bytes, pos).unwrap_or(0) as usize;
    let end = pos.saturating_add(len).min(bytes.len());
    dest.clear();
    dest.push_str(std::str::from_utf8(&bytes[*pos..end]).unwrap_or(""));
    *pos = end;
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

impl Model {
    pub fn new(plan: Plan) -> Model {
        let mut steps = Vec::new();
        let mut srcs = vec![0usize];
        for s in &plan.steps {
            steps.push(match &s.kind {
                Kind::Cat => StepState::Cat(Box::new(CatState {
                    ppm: Ppm::default(),
                    intern: Interner::default(),
                })),
                Kind::Str { recent, .. } => {
                    let st = StrState {
                        limit: *recent as usize,
                        srcs: srcs.clone(),
                        tuple_ppm: Ppm::default(),
                        tuples: Interner::default(),
                        ref_ppm: Ppm::default(),
                        trim: UintModel::default(),
                        chars: CharModel::new(2),
                        ex_ppm: Ppm::default(),
                        run_ppm: Ppm::default(),
                        run_readings: Interner::default(),
                        read_chars: CharModel::new(2),
                        recent: Vec::new(),
                        group_plan: None,
                        group_exact: 0,
                        align_stats: [0.0; 4],
                        seg_ppm: Ppm::default(),
                        segs: Interner::default(),
                        known: HashSet::default(),
                    };
                    srcs.push(s.col);
                    StepState::Str(Box::new(st))
                }
                Kind::Lid { .. } => StepState::Lid(Box::new(LidState {
                    seen: FastMap::default(),
                    order: BTreeMap::new(),
                    mode_ppm: Ppm::default(),
                    delta: UintModel::default(),
                    direct: UintModel::default(),
                    from_last: UintModel::default(),
                    lo_ppm: Ppm::default(),
                    hi_ppm: Ppm::default(),
                    irregular: Flag::default(),
                    partner_bad: Flag::default(),
                    last_new: 0,
                    stats: [0.0; 6],
                    mode_count: [0; LID_MODE_COUNT],
                })),
                Kind::Num { .. } => StepState::Num(Box::new(NumState {
                    ppm: Ppm::default(),
                    intern: Interner::default(),
                    uint: UintModel::default(),
                    irregular: Flag::default(),
                    d_ppm: Ppm::default(),
                    d_intern: Interner::default(),
                    d_uint: UintModel::default(),
                    refs: FastMap::default(),
                    novel: (0.0, 0),
                })),
            });
        }
        let p = plan.pos_cols;
        let n_steps = plan.steps.len();
        let row = RowState {
            fields: vec![String::new(); plan.n_cols],
            coded_text: String::new(),
            key_buf: Vec::new(),
            raw_chars: CharModel::new(2),
            cat_chars: CharModel::new(1),
            pos_hash: 0,
            pos_first_hash: 0,
            pos_first_two_hash: 0,
            pos_sym: NOVEL,
            prev_pos: NOVEL,
            last_char: 0,
            first_in_group: true,
            prev_num: 0,
            num_seen: false,
            lid_ref_key: 0,
            cat_group_id: 0,
            surf_hash: 0,
            str_shape: 0,
            str_group_id: 0,
            prev_str_group_id: 0,
            prev_cat_group_id: 0,
            row_irregular: true,
            rebuilt: String::new(),
            script_runs: Vec::new(),
            reading_spans: Vec::new(),
        };
        Model {
            steps,
            row_state: row,
            pos_ppm: Ppm::default(),
            pos_intern: Interner::default(),
            comp_ppm: (0..p).map(|_| Ppm::default()).collect(),
            comp_intern: (0..p).map(|_| Interner::default()).collect(),
            comp_chars: CharModel::new(1),
            pos_hashes: Vec::new(),
            cat_group: CatGroup {
                ppm: Ppm::default(),
                tuples: Interner::default(),
                ctx_col: plan.cat_ctx,
                steps: plan
                    .steps
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.kind == Kind::Cat)
                    .map(|(i, _)| i)
                    .collect(),
            },
            str_group: StrGroup {
                ppm: Ppm::default(),
                vecs: Interner::default(),
                steps: plan
                    .steps
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| matches!(s.kind, Kind::Str { .. }))
                    .map(|(i, _)| i)
                    .collect(),
            },
            row_flag: Flag::default(),
            bundle_cats: true,
            active: None,
            step_bits: vec![0.0; n_steps],
            pos_bits: 0.0,
            plan,
        }
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn set_active(&mut self, mask: Option<Vec<bool>>, bundle_cats: bool) {
        self.active = mask;
        self.bundle_cats = bundle_cats;
    }

    pub fn set_srcs(&mut self, si: usize, srcs: Vec<usize>) {
        if let StepState::Str(st) = &mut self.steps[si] {
            st.srcs = srcs;
        }
    }

    pub fn lid_stats(&self) -> Option<([f64; 6], [u64; LID_MODE_COUNT])> {
        self.steps.iter().find_map(|s| match s {
            StepState::Lid(l) => Some((l.stats, l.mode_count)),
            _ => None,
        })
    }

    pub fn align_stats(&self) -> Vec<(usize, [f64; 4])> {
        self.steps
            .iter()
            .zip(&self.plan.steps)
            .filter_map(|(s, p)| match s {
                StepState::Str(st) if st.align_stats[1] + st.align_stats[3] > 0.0 => {
                    Some((p.col, st.align_stats))
                }
                _ => None,
            })
            .collect()
    }

    pub fn num_stats(&self) -> Vec<(f64, u64)> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                StepState::Num(n) => Some(n.novel),
                _ => None,
            })
            .collect()
    }

    pub fn row(&self) -> &[String] {
        &self.row_state.fields
    }

    pub fn start_group(&mut self) {
        self.row_state.first_in_group = true;
    }

    pub fn code_row(&mut self, coder: &mut Coder, fields: &[&str], surface: &str) {
        let row = &mut self.row_state;
        if coder.is_decoder() {
            row.fields[0].clear();
            row.fields[0].push_str(surface);
        } else {
            for (i, f) in fields.iter().enumerate() {
                row.fields[i].clear();
                row.fields[i].push_str(f);
            }
        }
        row.last_char = row.fields[0].chars().next_back().map_or(0, |ch| ch as u32);
        row.prev_num = 0;
        row.num_seen = false;
        row.lid_ref_key = 0;
        let before = coder.bits;
        if self.plan.pos_cols > 0 {
            code_pos(
                &mut self.pos_ppm,
                &mut self.pos_intern,
                &mut self.comp_ppm,
                &mut self.comp_intern,
                &mut self.comp_chars,
                &mut self.pos_hashes,
                row,
                coder,
                self.plan.pos_cols,
                self.active.is_some(),
            );
        }
        self.pos_bits += coder.bits - before;
        row.cat_group_id = 0;
        row.str_shape = 0;
        row.str_group_id = 0;
        row.surf_hash = hash_bytes(row.fields[0].as_bytes());
        let bundle = self.bundle_cats && !self.cat_group.steps.is_empty();
        if bundle {
            self.cat_group.ppm.prefetch(mix(
                mix(9 + u64::from(row.first_in_group), row.pos_hash),
                u64::from(row.prev_cat_group_id),
            ));
            self.cat_group.ppm.prefetch(mix(5, row.pos_hash));
        }
        row.row_irregular = true;
        if self.active.is_none() {
            let mut irr = false;
            if !coder.is_decoder() {
                for step in &self.plan.steps {
                    irr |= match &step.kind {
                        Kind::Num { .. } => canonical_i64(&row.fields[step.col]).is_none(),
                        Kind::Lid(l) => match canonical_u64(&row.fields[step.col]) {
                            None => true,
                            Some(v) => l.partner.is_some_and(|p| {
                                canonical_u64(&row.fields[p]) != Some(v >> l.shift)
                            }),
                        },
                        _ => false,
                    };
                }
            }
            let has_numbers = self
                .plan
                .steps
                .iter()
                .any(|s| matches!(s.kind, Kind::Num { .. } | Kind::Lid(_)));
            row.row_irregular = has_numbers && self.row_flag.code(coder, irr, IRREGULAR_FLAG_LIMIT);
        }
        let sbundle = self.active.is_none() && !self.str_group.steps.is_empty();
        if sbundle {
            self.str_group.ppm.prefetch(mix(
                mix(66 + u64::from(row.first_in_group), row.pos_hash),
                u64::from(row.prev_str_group_id),
            ));
        }
        for (si, step) in self.plan.steps.iter().enumerate() {
            if self.active.as_ref().is_some_and(|m| !m[si]) {
                if let Kind::Num { .. } = step.kind
                    && let Some(v) = canonical_i64(&row.fields[step.col])
                {
                    note_num(row, v);
                }
                continue;
            }
            let before = coder.bits;
            match (&mut self.steps[si], &step.kind) {
                (StepState::Cat(st), Kind::Cat) => {
                    if !bundle {
                        code_cat(st, row, coder, step.col, 0);
                    } else if si == self.cat_group.steps[0] {
                        code_cat_group(
                            &mut self.cat_group,
                            &mut self.steps,
                            &self.plan.steps,
                            row,
                            coder,
                        );
                    }
                }
                (StepState::Str(_), Kind::Str { align, .. }) => {
                    if sbundle && si == self.str_group.steps[0] {
                        code_str_group(
                            &mut self.str_group,
                            &mut self.steps,
                            &self.plan.steps,
                            row,
                            coder,
                        );
                    }
                    if let StepState::Str(st) = &mut self.steps[si] {
                        code_str(
                            st,
                            row,
                            coder,
                            step.col,
                            StrOptions {
                                align: *align,
                                grouped: sbundle,
                            },
                        );
                    }
                }
                (StepState::Lid(st), Kind::Lid(spec)) => code_lid(st, row, coder, step.col, spec),
                (StepState::Num(st), Kind::Num { mode }) => {
                    code_num(st, row, coder, step.col, *mode);
                }
                _ => unreachable!(),
            }
            self.step_bits[si] += coder.bits - before;
        }
        row.prev_pos = row.pos_sym;
        row.first_in_group = false;
        row.prev_str_group_id = row.str_group_id;
        row.prev_cat_group_id = row.cat_group_id;
    }
}

fn note_num(row: &mut RowState, v: i64) {
    if !row.num_seen {
        row.num_seen = true;
        row.prev_num = v;
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "codec step state is passed as separate borrows"
)]
fn code_pos(
    ppm: &mut Ppm,
    intern: &mut Interner,
    comp_ppm: &mut [Ppm],
    comp_intern: &mut [Interner],
    comp_chars: &mut CharModel,
    hashes: &mut Vec<(u64, u64, u64)>,
    row: &mut RowState,
    coder: &mut Coder,
    pos_cols: usize,
    trial: bool,
) {
    let dec = coder.is_decoder();
    row.key_buf.clear();
    if !dec {
        for i in 0..pos_cols {
            put_str(&mut row.key_buf, &row.fields[FIXED_COLS + i]);
        }
    }
    let sym = if dec {
        NOVEL
    } else {
        intern.find(&row.key_buf).unwrap_or(NOVEL)
    };
    let id = if trial {
        intern.find_or_add(&row.key_buf)
    } else {
        let ctxs = [
            mix(
                mix(100 + u64::from(row.first_in_group), u64::from(row.prev_pos)),
                u64::from(row.last_char),
            ),
            mix(101, u64::from(row.last_char)),
            102,
        ];
        let id = if let Some(id) = ppm.code(coder, &ctxs, sym) {
            id
        } else {
            let mut h = 0u64;
            row.key_buf.clear();
            for i in 0..pos_cols {
                let ctxs = [mix(110 + i as u64, h), 120 + i as u64];
                let fsym = if dec {
                    NOVEL
                } else {
                    comp_intern[i]
                        .find(row.fields[FIXED_COLS + i].as_bytes())
                        .unwrap_or(NOVEL)
                };
                let fid = if let Some(f) = comp_ppm[i].code(coder, &ctxs, fsym) {
                    f
                } else {
                    comp_chars.code_str(
                        coder,
                        &[],
                        &row.fields[FIXED_COLS + i],
                        &mut row.coded_text,
                    );
                    comp_intern[i].find_or_add(row.coded_text.as_bytes())
                };
                comp_ppm[i].update(fid);
                let f = std::str::from_utf8(comp_intern[i].get(fid))
                    .unwrap_or("")
                    .to_string();
                h = mix(h, hash_bytes(f.as_bytes()));
                put_str(&mut row.key_buf, &f);
                row.fields[FIXED_COLS + i] = f;
            }
            intern.find_or_add(&row.key_buf)
        };
        ppm.update(id);
        id
    };
    row.pos_sym = id;
    if dec {
        let tuple = intern.get(id);
        let mut pos = 0;
        for i in 0..pos_cols {
            read_str(tuple, &mut pos, &mut row.fields[FIXED_COLS + i]);
        }
    }
    if let Some(&(pos_first_hash, pos_first_two_hash, all)) =
        hashes.get(id as usize).filter(|x| x.2 != 0)
    {
        (row.pos_first_hash, row.pos_first_two_hash, row.pos_hash) =
            (pos_first_hash, pos_first_two_hash, all);
        return;
    }
    let mut h = 0u64;
    for i in 0..pos_cols {
        h = mix(h, hash_bytes(row.fields[FIXED_COLS + i].as_bytes()));
        if i == 0 {
            row.pos_first_hash = h;
        }
        if i == 1 {
            row.pos_first_two_hash = h;
        }
    }
    if pos_cols == 1 {
        row.pos_first_two_hash = row.pos_first_hash;
    }
    row.pos_hash = h | 1;
    if hashes.len() <= id as usize {
        hashes.resize(id as usize + 1, (0, 0, 0));
    }
    hashes[id as usize] = (row.pos_first_hash, row.pos_first_two_hash, row.pos_hash);
}

fn code_cat(st: &mut CatState, row: &mut RowState, coder: &mut Coder, col: usize, prev: u64) {
    let dec = coder.is_decoder();
    let mut ctxs = [0u64; 4];
    let mut ctx_count = 0;
    if prev != 0 {
        ctxs[ctx_count] = mix(mix(1, row.pos_hash), prev);
        ctx_count += 1;
    }
    ctxs[ctx_count] = mix(2, row.pos_hash);
    ctxs[ctx_count + 1] = mix(3, row.pos_first_hash);
    ctxs[ctx_count + 2] = 4;
    ctx_count += 3;
    let sym = if dec {
        NOVEL
    } else {
        st.intern.find(row.fields[col].as_bytes()).unwrap_or(NOVEL)
    };
    let id = if let Some(id) = st.ppm.code(coder, &ctxs[..ctx_count], sym) {
        id
    } else {
        row.cat_chars
            .code_str(coder, &[], &row.fields[col], &mut row.coded_text);
        st.intern.find_or_add(row.coded_text.as_bytes())
    };
    st.ppm.update(id);
    if dec {
        row.fields[col].clear();
        row.fields[col].push_str(std::str::from_utf8(st.intern.get(id)).unwrap_or(""));
    }
}

fn code_cat_group(
    g: &mut CatGroup,
    steps: &mut [StepState],
    plan_steps: &[Step],
    row: &mut RowState,
    coder: &mut Coder,
) {
    let dec = coder.is_decoder();
    row.key_buf.clear();
    if !dec {
        for &si in &g.steps {
            let col = plan_steps[si].col;
            put_str(&mut row.key_buf, &row.fields[col]);
        }
    }
    let sym = if dec {
        NOVEL
    } else {
        g.tuples.find(&row.key_buf).unwrap_or(NOVEL)
    };
    let colh = g
        .ctx_col
        .map_or(0, |k| hash_bytes(row.fields[k].as_bytes()));
    let ctxs = [
        mix(mix(8, row.pos_hash), colh),
        mix(
            mix(9 + u64::from(row.first_in_group), row.pos_hash),
            u64::from(row.prev_cat_group_id),
        ),
        mix(5, row.pos_hash),
        mix(6, row.pos_first_hash),
        7,
    ];
    let ctxs = if g.ctx_col.is_some() {
        &ctxs[..]
    } else {
        &ctxs[1..]
    };
    let id = if let Some(id) = g.ppm.code(coder, ctxs, sym) {
        id
    } else {
        let mut prev = 0u64;
        for &si in &g.steps {
            let col = plan_steps[si].col;
            if let StepState::Cat(st) = &mut steps[si] {
                code_cat(st, row, coder, col, prev);
            }
            prev = mix(prev | 1, hash_bytes(row.fields[col].as_bytes()));
        }
        row.key_buf.clear();
        for &si in &g.steps {
            let col = plan_steps[si].col;
            put_str(&mut row.key_buf, &row.fields[col]);
        }
        g.tuples.find_or_add(&row.key_buf)
    };
    g.ppm.update(id);
    row.cat_group_id = id + 1;
    if dec {
        let tuple = g.tuples.get(id);
        let mut pos = 0;
        for &si in &g.steps {
            read_str(tuple, &mut pos, &mut row.fields[plan_steps[si].col]);
        }
    }
}

fn split_script_runs(s: &str, out: &mut Vec<(bool, usize, usize)>) {
    out.clear();
    for (i, ch) in s.char_indices() {
        let kana = is_kana(ch);
        match out.last_mut() {
            Some(last) if last.0 == kana => last.2 = i + ch.len_utf8(),
            _ => out.push((kana, i, i + ch.len_utf8())),
        }
    }
}

fn match_reading(
    surface: &str,
    script_runs: &[(bool, usize, usize)],
    target: &str,
    reading_spans: &mut Vec<(usize, usize)>,
) -> bool {
    fn rec(
        surface: &str,
        script_runs: &[(bool, usize, usize)],
        target: &str,
        pos: usize,
        reading_spans: &mut Vec<(usize, usize)>,
    ) -> bool {
        let Some(&(is_kana_run, run_start, run_end)) = script_runs.first() else {
            return pos == target.len();
        };
        if is_kana_run {
            let mut p = pos;
            let mut t = target[pos..].chars();
            for ch in surface[run_start..run_end].chars() {
                match t.next() {
                    Some(x) if x == kata(ch) => p += x.len_utf8(),
                    _ => return false,
                }
            }
            return rec(surface, &script_runs[1..], target, p, reading_spans);
        }
        let mut end = pos;
        for ch in target[pos..].chars() {
            end += ch.len_utf8();
            reading_spans.push((pos, end));
            if rec(surface, &script_runs[1..], target, end, reading_spans) {
                return true;
            }
            reading_spans.pop();
        }
        false
    }
    reading_spans.clear();
    rec(surface, script_runs, target, 0, reading_spans)
}

fn common_prefix_len(a: impl Iterator<Item = char>, b: impl Iterator<Item = char>) -> usize {
    a.zip(b).take_while(|(x, y)| x == y).count()
}

#[inline]
fn resolve_source<'a>(st: &'a StrState, fields: &'a [String], index: usize) -> &'a str {
    let source_count = st.srcs.len();
    if index < source_count {
        fields[st.srcs[index]].as_str()
    } else {
        st.recent
            .get(index - source_count)
            .map_or("", |x| x.as_str())
    }
}

fn apply_ref(src: &str, to_kata: bool, trim: usize, append: &str, out: &mut String) {
    out.clear();
    let end = if trim == 0 {
        src.len()
    } else {
        src.char_indices().rev().nth(trim - 1).map_or(0, |(i, _)| i)
    };
    let head = &src[..end];
    if to_kata {
        out.extend(head.chars().map(kata));
    } else {
        out.push_str(head);
    }
    out.push_str(append);
}

type Cand = (usize, usize, usize, usize);

fn plan_str(st: &StrState, row: &mut RowState, col: usize, align: bool) -> (u32, Cand) {
    if align {
        split_script_runs(&row.fields[0], &mut row.script_runs);
        if match_reading(
            &row.fields[0],
            &row.script_runs,
            &row.fields[col],
            &mut row.reading_spans,
        ) {
            return (EX_ALIGNED, (0, 0, 0, 0));
        }
    }
    let nsrc = st.srcs.len();
    let target = &row.fields[col];
    let tlen = target.chars().count();
    let mut best: Option<(usize, usize, usize, usize, usize)> = None;
    let mut consider = |cand: (usize, usize, usize, usize, usize)| {
        if best.is_none_or(|b| (cand.0, cand.1, cand.2, cand.3) < (b.0, b.1, b.2, b.3)) {
            best = Some(cand);
        }
    };
    for (j, &sc) in st.srcs.iter().enumerate() {
        let src = &row.fields[sc];
        let has_hira = src.chars().any(|ch| kata(ch) != ch);
        let slen = src.chars().count();
        for t in 0..=usize::from(has_hira) {
            let cp = if t == 0 {
                common_prefix_len(src.chars(), target.chars())
            } else {
                common_prefix_len(src.chars().map(kata), target.chars())
            };
            consider((tlen - cp, slen - cp, j, t, cp));
        }
    }
    for (r, src) in st.recent.iter().enumerate() {
        let cp = common_prefix_len(src.chars(), target.chars());
        consider((tlen - cp, src.chars().count() - cp, nsrc + r, 0, cp));
    }
    let (alen, trim, j, t, cp) = best.unwrap_or((0, 0, 0, 0, 0));
    let app = target
        .char_indices()
        .nth(cp)
        .map_or(target.len(), |(i, _)| i);
    let ex = if alen == 0 && trim == 0 {
        1 + (j * 2 + t) as u32
    } else {
        0
    };
    (ex, (j, t, trim, app))
}

fn remember_value(st: &mut StrState, v: &str) {
    remember(&mut st.recent, st.limit, v);
}

struct StrGroup {
    ppm: Ppm,
    vecs: Interner,
    steps: Vec<usize>,
}

fn code_str_group(
    g: &mut StrGroup,
    steps: &mut [StepState],
    plan_steps: &[Step],
    row: &mut RowState,
    coder: &mut Coder,
) {
    let dec = coder.is_decoder();
    row.key_buf.clear();
    if !dec {
        for &si in &g.steps {
            if let (StepState::Str(st), Kind::Str { align, .. }) =
                (&mut steps[si], &plan_steps[si].kind)
            {
                let (ex, cand) = plan_str(st, row, plan_steps[si].col, *align);
                st.group_plan = Some((ex, cand));
                row.key_buf.extend_from_slice(&(ex as u16).to_le_bytes());
            }
        }
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
        row.key_buf.clear();
        for &si in &g.steps {
            if let StepState::Str(st) = &mut steps[si] {
                let want = st.group_plan.map_or(0, |p| p.0);
                let ex = code_ex(st, row, coder, want);
                st.group_plan = st.group_plan.map(|p| (ex, p.1));
                st.group_exact = ex;
                row.key_buf.extend_from_slice(&(ex as u16).to_le_bytes());
            }
        }
        g.vecs.find_or_add(&row.key_buf)
    };
    g.ppm.update(id);
    row.str_group_id = id + 1;
    let b = g.vecs.get(id);
    for (k, &si) in g.steps.iter().enumerate() {
        if let StepState::Str(st) = &mut steps[si] {
            st.group_exact = b
                .get(2 * k..2 * k + 2)
                .map_or(0, |x| u32::from(u16::from_le_bytes([x[0], x[1]])));
            if st.group_exact == 0 {
                st.tuple_ppm
                    .prefetch(mix(mix(24, row.pos_hash), u64::from(row.str_group_id)));
                st.tuple_ppm.prefetch(mix(20, row.pos_hash));
            }
        }
    }
}

fn code_ex(st: &mut StrState, row: &mut RowState, coder: &mut Coder, want: u32) -> u32 {
    let ctxs = [mix(63, row.pos_hash), mix(64, row.pos_first_hash), 65];
    let ex = match st.ex_ppm.code(coder, &ctxs, want) {
        Some(e) => e,
        None => coder.uniform(u64::from(want), EX_DOMAIN) as u32,
    };
    st.ex_ppm.update(ex);
    ex
}

fn remember(recent: &mut Vec<String>, limit: usize, v: &str) {
    if limit == 0 {
        return;
    }
    if let Some(i) = recent.iter().position(|x| x == v) {
        recent[..=i].rotate_right(1);
        return;
    }
    let mut buf = if recent.len() >= limit {
        recent.pop().unwrap_or_default()
    } else {
        String::new()
    };
    buf.clear();
    buf.push_str(v);
    recent.insert(0, buf);
}

#[derive(Clone, Copy)]
struct StrOptions {
    align: bool,
    grouped: bool,
}

fn code_str(
    st: &mut StrState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    options: StrOptions,
) {
    let dec = coder.is_decoder();
    let pending = st.group_plan.take();
    let (ex, cand) = if options.grouped {
        (st.group_exact, pending.map_or((0, 0, 0, 0), |p| p.1))
    } else {
        let (want, cand) = if dec {
            (0, (0, 0, 0, 0))
        } else {
            plan_str(st, row, col, options.align)
        };
        (code_ex(st, row, coder, want), cand)
    };
    row.str_shape = mix(row.str_shape, u64::from(ex));
    if ex == EX_ALIGNED && options.align {
        code_aligned(st, row, coder, col);
    } else if ex != 0 && ex != EX_ALIGNED {
        if dec {
            let (j, t) = (((ex - 1) / 2) as usize, ((ex - 1) % 2) as usize);
            let src = resolve_source(st, &row.fields, j);
            apply_ref(src, t == 1, 0, "", &mut row.rebuilt);
            std::mem::swap(&mut row.fields[col], &mut row.rebuilt);
        }
    } else {
        code_ref(st, row, coder, col, cand);
    }
    remember_value(st, &row.fields[col]);
}

fn pair_hash(ch: char, seg: &[char]) -> u64 {
    let mut h = mix(0x5E6, ch as u64);
    for &x in seg {
        h = mix(h, x as u64);
    }
    h
}

fn segment_reading(st: &StrState, chars: &[char], reading: &[char]) -> Vec<(usize, usize)> {
    let (char_count, reading_len) = (chars.len(), reading.len());
    let stride = reading_len + 1;
    let unreachable_cost = f64::MAX / 4.0;
    let mut cost = vec![unreachable_cost; (char_count + 1) * stride];
    let mut segment_len_at = vec![0usize; (char_count + 1) * stride];
    cost[0] = 0.0;
    for i in 0..char_count {
        for reading_pos in 0..=reading_len {
            let base = cost[i * stride + reading_pos];
            if base >= unreachable_cost {
                continue;
            }
            let longest = if i + 1 == char_count {
                reading_len - reading_pos
            } else {
                MAX_SEGMENT_LEN.min(reading_len - reading_pos)
            };
            for len in 0..=longest {
                let known = st.known.contains(&pair_hash(
                    chars[i],
                    &reading[reading_pos..reading_pos + len],
                ));
                let segment_cost = if known {
                    KNOWN_SEGMENT_COST
                } else {
                    NEW_SEGMENT_BASE_COST + NEW_SEGMENT_COST_PER_CHAR * len as f64
                };
                let idx = (i + 1) * stride + reading_pos + len;
                if base + segment_cost < cost[idx] {
                    cost[idx] = base + segment_cost;
                    segment_len_at[idx] = len;
                }
            }
        }
    }
    let mut out = vec![(0, 0); char_count];
    let mut reading_pos = reading_len;
    for i in (0..char_count).rev() {
        let len = segment_len_at[(i + 1) * stride + reading_pos];
        out[i] = (reading_pos - len, reading_pos);
        reading_pos -= len;
    }
    out
}

fn code_segments(st: &mut StrState, coder: &mut Coder, run: &str, reading: &str, out: &mut String) {
    let dec = coder.is_decoder();
    let chars: Vec<char> = run.chars().collect();
    let rchars: Vec<char> = reading.chars().collect();
    let ranges = if dec {
        Vec::new()
    } else {
        segment_reading(st, &chars, &rchars)
    };
    out.clear();
    let mut piece = String::new();
    for (i, &ch) in chars.iter().enumerate() {
        let (seg, sym) = if dec {
            (&[][..], NOVEL)
        } else {
            let seg = &rchars[ranges[i].0..ranges[i].1];
            piece.clear();
            piece.extend(seg.iter());
            (seg, st.segs.find(piece.as_bytes()).unwrap_or(NOVEL))
        };
        let ctxs = [mix(80, ch as u64), 81];
        let id = if let Some(id) = st.seg_ppm.code(coder, &ctxs, sym) {
            id
        } else {
            let want = piece.clone();
            st.read_chars.code_str(coder, &[], &want, &mut piece);
            st.segs.find_or_add(piece.as_bytes())
        };
        st.seg_ppm.update(id);
        let text = std::str::from_utf8(st.segs.get(id)).unwrap_or("");
        let _ = seg;
        out.push_str(text);
        let seg_chars: Vec<char> = text.chars().collect();
        st.known.insert(pair_hash(ch, &seg_chars));
    }
}

fn code_aligned(st: &mut StrState, row: &mut RowState, coder: &mut Coder, col: usize) {
    let dec = coder.is_decoder();
    split_script_runs(&row.fields[0], &mut row.script_runs);
    if !dec {
        match_reading(
            &row.fields[0],
            &row.script_runs,
            &row.fields[col],
            &mut row.reading_spans,
        );
    }
    row.rebuilt.clear();
    let mut gi = 0;
    for &(is_kana_run, run_start, run_end) in &row.script_runs {
        if is_kana_run {
            row.rebuilt
                .extend(row.fields[0][run_start..run_end].chars().map(kata));
            continue;
        }
        let run = &row.fields[0][run_start..run_end];
        let reading = if dec {
            ""
        } else {
            &row.fields[col][row.reading_spans[gi].0..row.reading_spans[gi].1]
        };
        gi += 1;
        let sym = if dec {
            NOVEL
        } else {
            st.run_readings.find(reading.as_bytes()).unwrap_or(NOVEL)
        };
        let b0 = coder.bits;
        let hit = st
            .run_ppm
            .code(coder, &[mix(12, hash_bytes(run.as_bytes()))], sym);
        let b1 = coder.bits;
        if hit.is_some() {
            st.align_stats[0] += b1 - b0;
            st.align_stats[1] += 1.0;
        }
        let id = if let Some(id) = hit {
            id
        } else {
            code_segments(st, coder, run, reading, &mut row.coded_text);
            st.align_stats[2] += coder.bits - b0;
            st.align_stats[3] += 1.0;
            st.run_readings.find_or_add(row.coded_text.as_bytes())
        };
        st.run_ppm.update(id);
        row.rebuilt
            .push_str(std::str::from_utf8(st.run_readings.get(id)).unwrap_or(""));
    }
    if dec {
        std::mem::swap(&mut row.fields[col], &mut row.rebuilt);
    }
}

fn code_ref(st: &mut StrState, row: &mut RowState, coder: &mut Coder, col: usize, cand: Cand) {
    let dec = coder.is_decoder();
    let nsrc = st.srcs.len();
    let (bj, bt, btrim, bapp) = cand;
    let make_key = |key: &mut Vec<u8>, j: usize, t: usize, trim: usize, app: &str| {
        key.clear();
        key.push(j as u8);
        key.push(t as u8);
        put_varint(key, trim as u64);
        key.extend_from_slice(app.as_bytes());
    };
    let sym = if dec {
        NOVEL
    } else {
        let app = &row.fields[col][bapp..];
        make_key(&mut row.key_buf, bj, bt, btrim, app);
        st.tuples.find(&row.key_buf).unwrap_or(NOVEL)
    };
    let ctxs = [
        mix(mix(24, row.pos_hash), u64::from(row.str_group_id)),
        mix(20, row.pos_hash),
        mix(21, row.pos_first_hash),
        22,
    ];
    let ctxs = if row.str_group_id == 0 {
        &ctxs[1..]
    } else {
        &ctxs[..]
    };
    let id = if let Some(id) = st.tuple_ppm.code(coder, ctxs, sym) {
        id
    } else {
        let rsym = (bj * 2 + bt) as u32;
        let r = match st
            .ref_ppm
            .code(coder, &[mix(23, row.pos_first_hash), 24], rsym)
        {
            Some(r) => r,
            None => coder.uniform(u64::from(rsym), 2 * (nsrc + MAX_RECENT) as u64) as u32,
        };
        st.ref_ppm.update(r);
        let trim = st.trim.code(coder, btrim as u64) as usize;
        let app = if dec { "" } else { &row.fields[col][bapp..] };
        st.chars.code_str(coder, &[], app, &mut row.coded_text);
        let (j, t) = ((r / 2) as usize, (r % 2) as usize);
        let tmp = std::mem::take(&mut row.coded_text);
        make_key(&mut row.key_buf, j, t, trim, &tmp);
        row.coded_text = tmp;
        st.tuples.find_or_add(&row.key_buf)
    };
    st.tuple_ppm.update(id);
    if !dec {
        return;
    }
    let key = st.tuples.get(id);
    let (j, t) = (key[0] as usize, key[1] as usize);
    let mut pos = 2;
    let trim = get_varint(key, &mut pos).unwrap_or(0) as usize;
    let app = std::str::from_utf8(&key[pos..]).unwrap_or("");
    let src = resolve_source(st, &row.fields, j);
    apply_ref(src, t == 1, trim, app, &mut row.rebuilt);
    std::mem::swap(&mut row.fields[col], &mut row.rebuilt);
}

fn code_num(st: &mut NumState, row: &mut RowState, coder: &mut Coder, col: usize, mode: u8) {
    let dec = coder.is_decoder();
    let parsed = if dec {
        None
    } else {
        canonical_i64(&row.fields[col])
    };
    if row.row_irregular
        && st
            .irregular
            .code(coder, !dec && parsed.is_none(), IRREGULAR_FLAG_LIMIT)
    {
        row.raw_chars
            .code_str(coder, &[], &row.fields[col], &mut row.coded_text);
        if dec {
            row.fields[col] = row.coded_text.clone();
        }
        return;
    }
    let reference = if mode == 2 && row.lid_ref_key != 0 {
        st.refs.get(&row.lid_ref_key).copied()
    } else {
        None
    };
    if let Some((rcost, rlc)) = reference {
        let ctxs = [
            mix(mix(90, row.prev_num as u64), rlc as u64),
            mix(mix(91, row.prev_num as u64), row.pos_hash),
            mix(92, row.pos_hash),
            93,
        ];
        let want = parsed.map_or(0, |v| v.wrapping_sub(rcost));
        let sym = match parsed {
            Some(_) => st.d_intern.find(&want.to_le_bytes()).unwrap_or(NOVEL),
            None => NOVEL,
        };
        let id = if let Some(id) = st.d_ppm.code(coder, &ctxs, sym) {
            id
        } else {
            let b0 = coder.bits;
            let d = unzigzag(st.d_uint.code(coder, zigzag(want)));
            st.novel.0 += coder.bits - b0;
            st.novel.1 += 1;
            st.d_intern.find_or_add(&d.to_le_bytes())
        };
        st.d_ppm.update(id);
        let d = i64::from_le_bytes(st.d_intern.get(id).try_into().unwrap_or([0; 8]));
        let v = rcost.wrapping_add(d);
        st.refs.insert(row.lid_ref_key, (v, row.prev_num));
        note_num(row, v);
        if dec {
            set_i64(&mut row.fields[col], v);
        }
        return;
    }
    let ctxs: [u64; 6];
    let n;
    match mode {
        0 => {
            ctxs = [mix(50, row.pos_first_hash), 51, 0, 0, 0, 0];
            n = 2;
        }
        2 => {
            ctxs = [
                mix(mix(60, row.prev_num as u64), row.pos_hash),
                mix(61, row.pos_first_two_hash),
                62,
                0,
                0,
                0,
            ];
            n = 3;
        }
        1 => {
            let base = mix(u64::from(row.pos_sym), u64::from(row.cat_group_id));
            ctxs = [
                mix(mix(70, base), row.surf_hash),
                mix(71, base),
                mix(72, u64::from(row.pos_sym)),
                mix(73, row.pos_first_two_hash),
                74,
                0,
            ];
            n = 5;
        }
        _ => {
            let base = mix(
                mix(u64::from(row.pos_sym), u64::from(row.cat_group_id)),
                row.prev_num as u64,
            );
            ctxs = [
                mix(mix(80, base), row.surf_hash),
                mix(81, base),
                mix(82, mix(u64::from(row.pos_sym), u64::from(row.cat_group_id))),
                mix(83, u64::from(row.pos_sym)),
                mix(84, row.pos_first_two_hash),
                85,
            ];
            n = 6;
        }
    }
    let skip = usize::from((mode == 1 || mode == 3) && st.ppm.distinct(ctxs[1]) < 2);
    let ctxs = &ctxs[skip..n];
    let sym = match parsed {
        Some(v) => st.intern.find(&v.to_le_bytes()).unwrap_or(NOVEL),
        None => NOVEL,
    };
    let id = if let Some(id) = st.ppm.code(coder, ctxs, sym) {
        id
    } else {
        let b0 = coder.bits;
        let v = unzigzag(st.uint.code(coder, zigzag(parsed.unwrap_or(0))));
        st.novel.0 += coder.bits - b0;
        st.novel.1 += 1;
        st.intern.find_or_add(&v.to_le_bytes())
    };
    st.ppm.update(id);
    let v = i64::from_le_bytes(st.intern.get(id).try_into().unwrap_or([0; 8]));
    if mode == 2 && row.lid_ref_key != 0 {
        st.refs.insert(row.lid_ref_key, (v, row.prev_num));
    }
    note_num(row, v);
    if dec {
        set_i64(&mut row.fields[col], v);
    }
}

fn neighbours(
    order: &BTreeMap<Vec<u8>, (u64, u64)>,
    key: &[u8],
) -> (Option<u64>, Option<(u64, u64)>) {
    if let Some(e) = order.get(key) {
        return (Some(e.1), None);
    }
    let pred = order
        .range::<[u8], _>((Bound::Unbounded, Bound::Excluded(key)))
        .next_back()
        .map(|e| (e.1).1);
    let succ = order
        .range::<[u8], _>((Bound::Excluded(key), Bound::Unbounded))
        .next()
        .map(|e| (e.1).0);
    match (pred, succ) {
        (Some(p), Some(s)) if p < s => (None, Some((p + 1, s))),
        _ => (None, None),
    }
}

const DIGITS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

fn set_u64(s: &mut String, v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = v;
    while v >= 100_000_000 {
        let mut low = (v % 100_000_000) as u32;
        v /= 100_000_000;
        for _ in 0..4 {
            let d = (low % 100) as usize * 2;
            low /= 100;
            i -= 2;
            buf[i..i + 2].copy_from_slice(&DIGITS[d..d + 2]);
        }
    }
    let mut v = v as u32;
    while v >= 100 {
        let d = (v % 100) as usize * 2;
        v /= 100;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGITS[d..d + 2]);
    }
    if v >= 10 {
        let d = v as usize * 2;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGITS[d..d + 2]);
    } else {
        i -= 1;
        buf[i] = b'0' + v as u8;
    }
    s.clear();
    s.push_str(std::str::from_utf8(&buf[i..]).unwrap_or(""));
}

fn set_i64(s: &mut String, v: i64) {
    if v < 0 {
        set_u64(s, v.unsigned_abs());
        s.insert(0, '-');
    } else {
        set_u64(s, v as u64);
    }
}

fn code_lid(st: &mut LidState, row: &mut RowState, coder: &mut Coder, col: usize, spec: &LidSpec) {
    let dec = coder.is_decoder();
    let (shift, partner, order) = (spec.shift, spec.partner, spec.order);
    let parsed = if dec {
        None
    } else {
        canonical_u64(&row.fields[col])
    };
    let irregular = row.row_irregular
        && st
            .irregular
            .code(coder, !dec && parsed.is_none(), IRREGULAR_FLAG_LIMIT);
    if irregular {
        row.raw_chars
            .code_str(coder, &[], &row.fields[col], &mut row.coded_text);
        if dec {
            row.fields[col].clone_from(&row.coded_text);
        }
    } else {
        let v = parsed.unwrap_or(0);
        let (id, low) = (v >> shift, v & ((1u64 << shift) - 1));
        let mut kh = mix(7, row.pos_first_hash);
        for &k in &spec.key {
            kh = mix(kh, hash_bytes(row.fields[k].as_bytes()));
        }
        let seen = st.seen.get(&kh).copied();
        let mut near = (None, None);
        let want_mode = if seen == Some(id) {
            0
        } else {
            if !dec && let Some(o) = order {
                near = neighbours(&st.order, row.fields[o].as_bytes());
            }
            let bits = |x: u64| 64 - u64::from(x.leading_zeros());
            if near.0.is_some() {
                2
            } else if near.1.is_some_and(|(lo, hi)| lo <= id && id < hi) {
                1
            } else if 2 * bits(id.abs_diff(st.last_new)) + 1 < bits(id) + 2 {
                4
            } else {
                3
            }
        };
        let b0 = coder.bits;
        let ctxs = [
            mix(mix(32, row.pos_first_hash), row.str_shape),
            mix(30, row.pos_first_hash),
            31,
        ];
        let mode = match st.mode_ppm.code(coder, &ctxs, want_mode) {
            Some(m) => m,
            None => coder.uniform(u64::from(want_mode), LID_MODE_COUNT as u64) as u32,
        };
        st.mode_ppm.update(mode);
        let b1 = coder.bits;
        st.stats[0] += b1 - b0;
        st.mode_count[mode as usize % LID_MODE_COUNT] += 1;
        if dec
            && (mode == 1 || mode == 2)
            && let Some(o) = order
        {
            near = neighbours(&st.order, row.fields[o].as_bytes());
        }
        let new_id = match mode {
            0 => seen.unwrap_or(0),
            1 => {
                let (lo, hi) = near.1.unwrap_or((0, 1));
                lo + coder.uniform(id.wrapping_sub(lo), hi - lo)
            }
            2 => {
                let a = near.0.unwrap_or(0) as i64;
                a.wrapping_add(unzigzag(
                    st.delta.code(coder, zigzag((id as i64).wrapping_sub(a))),
                )) as u64
            }
            4 => {
                let a = st.last_new as i64;
                a.wrapping_add(unzigzag(
                    st.from_last
                        .code(coder, zigzag((id as i64).wrapping_sub(a))),
                )) as u64
            }
            _ => st.direct.code(coder, id),
        };
        st.stats[1 + (mode as usize % LID_MODE_COUNT).min(3)] += coder.bits - b1;
        let b2 = coder.bits;
        if mode != 0 {
            st.last_new = new_id;
        }
        st.seen.insert(kh, new_id);
        if let Some(o) = order {
            let k = row.fields[o].as_bytes();
            match st.order.get_mut(k) {
                Some(e) => {
                    e.0 = e.0.min(new_id);
                    e.1 = e.1.max(new_id);
                }
                None => {
                    st.order.insert(k.to_vec(), (new_id, new_id));
                }
            }
        }
        let split = u32::from(shift.min(LO_BITS_MAX));
        let ctxs = [
            mix(mix(41, row.pos_hash), row.str_shape),
            mix(42, row.pos_hash),
            mix(43, row.pos_first_hash),
            44,
        ];
        let lo = low & ((1u64 << split) - 1);
        let lo = match st.lo_ppm.code(coder, &ctxs, lo as u32) {
            Some(x) => x,
            None => coder.raw_bits(lo, split) as u32,
        };
        st.lo_ppm.update(lo);
        let mut low = u64::from(lo);
        if u32::from(shift) > split {
            let hi_bits = u32::from(shift) - split;
            let hi = (v & ((1u64 << shift) - 1)) >> split;
            let colh = spec
                .low_col
                .map_or(0, |k| hash_bytes(row.fields[k].as_bytes()));
            let ctxs = [
                mix(mix(50, new_id), colh),
                mix(mix(51, row.pos_hash), row.str_shape),
                mix(52, row.pos_hash),
                mix(53, row.pos_first_hash),
                54,
            ];
            let hi = match st.hi_ppm.code(coder, &ctxs, hi as u32) {
                Some(x) => x,
                None => coder.raw_bits(hi, hi_bits) as u32,
            };
            st.hi_ppm.update(hi);
            low |= u64::from(hi) << split;
        }
        row.lid_ref_key = mix(mix(77, new_id), low >> LO_BITS_MAX) | 1;
        st.stats[5] += coder.bits - b2;
        if dec {
            set_u64(&mut row.fields[col], (new_id << shift) | low);
        }
        if let Some(p) = partner {
            let bad = row.row_irregular
                && st.partner_bad.code(
                    coder,
                    !dec && canonical_u64(&row.fields[p]) != Some(new_id),
                    IRREGULAR_FLAG_LIMIT,
                );
            if bad {
                row.raw_chars
                    .code_str(coder, &[], &row.fields[p], &mut row.coded_text);
                if dec {
                    row.fields[p].clone_from(&row.coded_text);
                }
            } else if dec {
                set_u64(&mut row.fields[p], new_id);
            }
        }
        return;
    }
    if let Some(p) = partner {
        row.raw_chars
            .code_str(coder, &[], &row.fields[p], &mut row.coded_text);
        if dec {
            row.fields[p].clone_from(&row.coded_text);
        }
    }
}
