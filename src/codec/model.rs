use super::coder::{Coder, Flag, UintModel};
use super::ppm::{FastMap, Interner, NOVEL, Ppm, hash_bytes, mix};
use super::text::{CharModel, is_kana, kata};
use crate::wire::{get_varint, put_varint, unzigzag, zigzag};
use std::collections::BTreeMap;
use std::ops::Bound;

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

fn get_opt(b: &[u8], pos: &mut usize) -> Option<Option<usize>> {
    let v = get_varint(b, pos)?;
    Some(if v == 0 { None } else { Some(v as usize - 1) })
}

impl Plan {
    pub fn write(&self, out: &mut Vec<u8>) {
        put_varint(out, self.n_cols as u64);
        put_varint(out, self.pos_cols as u64);
        put_opt(out, self.cat_ctx);
        put_varint(out, self.steps.len() as u64);
        for s in &self.steps {
            put_varint(out, s.col as u64);
            match &s.kind {
                Kind::Cat => out.push(0),
                Kind::Str { align, recent } => {
                    out.push(1 + *align as u8);
                    out.push(*recent);
                }
                Kind::Num { mode } => {
                    out.push(3);
                    out.push(*mode);
                }
                Kind::Lid(l) => {
                    out.push(4);
                    out.push(l.shift);
                    put_opt(out, l.partner);
                    put_opt(out, l.order);
                    put_opt(out, l.low_col);
                    put_varint(out, l.key.len() as u64);
                    for &k in &l.key {
                        put_varint(out, k as u64);
                    }
                }
            }
        }
    }

    pub fn read(b: &[u8], pos: &mut usize) -> Option<Plan> {
        let n_cols = get_varint(b, pos)? as usize;
        let pos_cols = get_varint(b, pos)? as usize;
        let cat_ctx = get_opt(b, pos)?;
        let n = get_varint(b, pos)? as usize;
        if n_cols > 1 << 20 || n > n_cols || FIXED_COLS + pos_cols > n_cols.max(FIXED_COLS) {
            return None;
        }
        let mut steps = Vec::new();
        for _ in 0..n {
            let col = get_varint(b, pos)? as usize;
            let tag = *b.get(*pos)?;
            *pos += 1;
            let kind = match tag {
                0 => Kind::Cat,
                1 | 2 => {
                    let recent = *b.get(*pos)?;
                    *pos += 1;
                    if recent as usize > MAX_RECENT {
                        return None;
                    }
                    Kind::Str {
                        align: tag == 2,
                        recent,
                    }
                }
                3 => {
                    let mode = *b.get(*pos)?;
                    *pos += 1;
                    Kind::Num { mode }
                }
                4 => {
                    let shift = *b.get(*pos)?;
                    *pos += 1;
                    let partner = get_opt(b, pos)?;
                    let order = get_opt(b, pos)?;
                    let low_col = get_opt(b, pos)?;
                    let kn = get_varint(b, pos)? as usize;
                    if kn > n_cols {
                        return None;
                    }
                    let mut key = Vec::new();
                    for _ in 0..kn {
                        key.push(get_varint(b, pos)? as usize);
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
                    if l.shift == 0 || l.shift > 54 {
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
    runs: Interner,
    read_chars: CharModel,
    recent: Vec<String>,
    limit: usize,
    group_plan: Option<(u32, Cand)>,
    group_exact: u32,
    align_stats: [f64; 4],
    seg_ppm: Ppm,
    segs: Interner,
    known: std::collections::HashSet<u64>,
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
    mode_count: [u64; 5],
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

struct Shared {
    cur: Vec<String>,
    tmp: String,
    scratch: Vec<u8>,
    raw_chars: CharModel,
    cat_chars: CharModel,
    pos_hash: u64,
    p0: u64,
    p01: u64,
    pos_sym: u32,
    prev_pos: u32,
    last_char: u32,
    first_in_group: bool,
    prev_num: i64,
    num_seen: bool,
    lid_key: u64,
    cat_id: u32,
    surf_hash: u64,
    shape: u64,
    sgroup: u32,
    prev_sgroup: u32,
    prev_cat_id: u32,
    row_irr: bool,
    out: String,
    runs: Vec<(bool, usize, usize)>,
    groups: Vec<(usize, usize)>,
}

pub struct Model {
    plan: Plan,
    steps: Vec<StepState>,
    sh: Shared,
    pos_ppm: Ppm,
    pos_intern: Interner,
    comp_ppm: Vec<Ppm>,
    comp_intern: Vec<Interner>,
    comp_chars: CharModel,
    pos_hashes: Vec<(u64, u64, u64)>,
    group: CatGroup,
    sgroup: StrGroup,
    row_flag: Flag,
    bundle_cats: bool,
    active: Option<Vec<bool>>,
    pub step_bits: Vec<f64>,
    pub pos_bits: f64,
}

pub(super) fn canonical_i64(s: &str) -> Option<i64> {
    let v: i64 = s.parse().ok()?;
    if v.unsigned_abs() < (1 << 61) && v.to_string() == s {
        Some(v)
    } else {
        None
    }
}

pub(super) fn canonical_u64(s: &str) -> Option<u64> {
    let v: u64 = s.parse().ok()?;
    if v < (1 << 62) && v.to_string() == s {
        Some(v)
    } else {
        None
    }
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
                        runs: Interner::default(),
                        read_chars: CharModel::new(2),
                        recent: Vec::new(),
                        group_plan: None,
                        group_exact: 0,
                        align_stats: [0.0; 4],
                        seg_ppm: Ppm::default(),
                        segs: Interner::default(),
                        known: Default::default(),
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
                    mode_count: [0; 5],
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
        let sh = Shared {
            cur: vec![String::new(); plan.n_cols],
            tmp: String::new(),
            scratch: Vec::new(),
            raw_chars: CharModel::new(2),
            cat_chars: CharModel::new(1),
            pos_hash: 0,
            p0: 0,
            p01: 0,
            pos_sym: NOVEL,
            prev_pos: NOVEL,
            last_char: 0,
            first_in_group: true,
            prev_num: 0,
            num_seen: false,
            lid_key: 0,
            cat_id: 0,
            surf_hash: 0,
            shape: 0,
            sgroup: 0,
            prev_sgroup: 0,
            prev_cat_id: 0,
            row_irr: true,
            out: String::new(),
            runs: Vec::new(),
            groups: Vec::new(),
        };
        Model {
            steps,
            sh,
            pos_ppm: Ppm::default(),
            pos_intern: Interner::default(),
            comp_ppm: (0..p).map(|_| Ppm::default()).collect(),
            comp_intern: (0..p).map(|_| Interner::default()).collect(),
            comp_chars: CharModel::new(1),
            pos_hashes: Vec::new(),
            group: CatGroup {
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
            sgroup: StrGroup {
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

    pub fn lid_stats(&self) -> Option<([f64; 6], [u64; 5])> {
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
        &self.sh.cur
    }

    pub fn start_group(&mut self) {
        self.sh.first_in_group = true;
    }

    pub fn code_row(&mut self, c: &mut Coder, fields: &[&str], surface: &str) {
        let sh = &mut self.sh;
        if !c.is_decoder() {
            for (i, f) in fields.iter().enumerate() {
                sh.cur[i].clear();
                sh.cur[i].push_str(f);
            }
        } else {
            sh.cur[0].clear();
            sh.cur[0].push_str(surface);
        }
        sh.last_char = sh.cur[0].chars().next_back().map_or(0, |ch| ch as u32);
        sh.prev_num = 0;
        sh.num_seen = false;
        sh.lid_key = 0;
        let before = c.bits;
        if self.plan.pos_cols > 0 {
            code_pos(
                &mut self.pos_ppm,
                &mut self.pos_intern,
                &mut self.comp_ppm,
                &mut self.comp_intern,
                &mut self.comp_chars,
                &mut self.pos_hashes,
                sh,
                c,
                self.plan.pos_cols,
                self.active.is_some(),
            );
        }
        self.pos_bits += c.bits - before;
        sh.cat_id = 0;
        sh.shape = 0;
        sh.sgroup = 0;
        sh.surf_hash = hash_bytes(sh.cur[0].as_bytes());
        let bundle = self.bundle_cats && !self.group.steps.is_empty();
        if bundle {
            self.group.ppm.prefetch(mix(
                mix(9 + sh.first_in_group as u64, sh.pos_hash),
                sh.prev_cat_id as u64,
            ));
            self.group.ppm.prefetch(mix(5, sh.pos_hash));
        }
        sh.row_irr = true;
        if self.active.is_none() {
            let mut irr = false;
            if !c.is_decoder() {
                for step in &self.plan.steps {
                    irr |= match &step.kind {
                        Kind::Num { .. } => canonical_i64(&sh.cur[step.col]).is_none(),
                        Kind::Lid(l) => match canonical_u64(&sh.cur[step.col]) {
                            None => true,
                            Some(v) => l
                                .partner
                                .is_some_and(|p| canonical_u64(&sh.cur[p]) != Some(v >> l.shift)),
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
            sh.row_irr = has_numbers && self.row_flag.code(c, irr, IRREGULAR_FLAG_LIMIT);
        }
        let sbundle = self.active.is_none() && !self.sgroup.steps.is_empty();
        if sbundle {
            self.sgroup.ppm.prefetch(mix(
                mix(66 + sh.first_in_group as u64, sh.pos_hash),
                sh.prev_sgroup as u64,
            ));
        }
        for (si, step) in self.plan.steps.iter().enumerate() {
            if self.active.as_ref().is_some_and(|m| !m[si]) {
                if let Kind::Num { .. } = step.kind
                    && let Some(v) = canonical_i64(&sh.cur[step.col])
                {
                    note_num(sh, v);
                }
                continue;
            }
            let before = c.bits;
            match (&mut self.steps[si], &step.kind) {
                (StepState::Cat(st), Kind::Cat) => {
                    if !bundle {
                        code_cat(st, sh, c, step.col, 0);
                    } else if si == self.group.steps[0] {
                        code_cat_group(&mut self.group, &mut self.steps, &self.plan.steps, sh, c);
                    }
                }
                (StepState::Str(_), Kind::Str { align, .. }) => {
                    if sbundle && si == self.sgroup.steps[0] {
                        code_str_group(&mut self.sgroup, &mut self.steps, &self.plan.steps, sh, c);
                    }
                    if let StepState::Str(st) = &mut self.steps[si] {
                        code_str(st, sh, c, step.col, *align, sbundle);
                    }
                }
                (StepState::Lid(st), Kind::Lid(spec)) => code_lid(st, sh, c, step.col, spec),
                (StepState::Num(st), Kind::Num { mode }) => code_num(st, sh, c, step.col, *mode),
                _ => unreachable!(),
            }
            self.step_bits[si] += c.bits - before;
        }
        sh.prev_pos = sh.pos_sym;
        sh.first_in_group = false;
        sh.prev_sgroup = sh.sgroup;
        sh.prev_cat_id = sh.cat_id;
    }
}

fn note_num(sh: &mut Shared, v: i64) {
    if !sh.num_seen {
        sh.num_seen = true;
        sh.prev_num = v;
    }
}

#[allow(clippy::too_many_arguments)]
fn code_pos(
    ppm: &mut Ppm,
    intern: &mut Interner,
    comp_ppm: &mut [Ppm],
    comp_intern: &mut [Interner],
    comp_chars: &mut CharModel,
    hashes: &mut Vec<(u64, u64, u64)>,
    sh: &mut Shared,
    c: &mut Coder,
    p: usize,
    trial: bool,
) {
    let dec = c.is_decoder();
    sh.scratch.clear();
    if !dec {
        for i in 0..p {
            put_str(&mut sh.scratch, &sh.cur[FIXED_COLS + i]);
        }
    }
    let sym = if dec {
        NOVEL
    } else {
        intern.find(&sh.scratch).unwrap_or(NOVEL)
    };
    let id = if trial {
        match intern.find(&sh.scratch) {
            Some(i) => i,
            None => intern.add(&sh.scratch),
        }
    } else {
        let ctxs = [
            mix(
                mix(100 + sh.first_in_group as u64, sh.prev_pos as u64),
                sh.last_char as u64,
            ),
            mix(101, sh.last_char as u64),
            102,
        ];
        let id = match ppm.code(c, &ctxs, sym) {
            Some(id) => id,
            None => {
                let mut h = 0u64;
                sh.scratch.clear();
                for i in 0..p {
                    let ctxs = [mix(110 + i as u64, h), 120 + i as u64];
                    let fsym = if dec {
                        NOVEL
                    } else {
                        comp_intern[i]
                            .find(sh.cur[FIXED_COLS + i].as_bytes())
                            .unwrap_or(NOVEL)
                    };
                    let fid = match comp_ppm[i].code(c, &ctxs, fsym) {
                        Some(f) => f,
                        None => {
                            comp_chars.code_str(c, &[], &sh.cur[FIXED_COLS + i], &mut sh.tmp);
                            match comp_intern[i].find(sh.tmp.as_bytes()) {
                                Some(f) => f,
                                None => comp_intern[i].add(sh.tmp.as_bytes()),
                            }
                        }
                    };
                    comp_ppm[i].update(fid);
                    let f = std::str::from_utf8(comp_intern[i].get(fid))
                        .unwrap_or("")
                        .to_string();
                    h = mix(h, hash_bytes(f.as_bytes()));
                    put_str(&mut sh.scratch, &f);
                    sh.cur[FIXED_COLS + i] = f;
                }
                match intern.find(&sh.scratch) {
                    Some(i) => i,
                    None => intern.add(&sh.scratch),
                }
            }
        };
        ppm.update(id);
        id
    };
    sh.pos_sym = id;
    if dec {
        let b = intern.get(id);
        let mut pos = 0;
        for i in 0..p {
            let n = get_varint(b, &mut pos).unwrap_or(0) as usize;
            let end = (pos + n).min(b.len());
            sh.cur[FIXED_COLS + i].clear();
            sh.cur[FIXED_COLS + i].push_str(std::str::from_utf8(&b[pos..end]).unwrap_or(""));
            pos = end;
        }
    }
    if let Some(&(p0, p01, all)) = hashes.get(id as usize).filter(|x| x.2 != 0) {
        (sh.p0, sh.p01, sh.pos_hash) = (p0, p01, all);
        return;
    }
    let mut h = 0u64;
    for i in 0..p {
        h = mix(h, hash_bytes(sh.cur[FIXED_COLS + i].as_bytes()));
        if i == 0 {
            sh.p0 = h;
        }
        if i == 1 {
            sh.p01 = h;
        }
    }
    if p == 1 {
        sh.p01 = sh.p0;
    }
    sh.pos_hash = h | 1;
    if hashes.len() <= id as usize {
        hashes.resize(id as usize + 1, (0, 0, 0));
    }
    hashes[id as usize] = (sh.p0, sh.p01, sh.pos_hash);
}

fn code_cat(st: &mut CatState, sh: &mut Shared, c: &mut Coder, col: usize, prev: u64) {
    let dec = c.is_decoder();
    let mut ctxs = [0u64; 4];
    let mut n = 0;
    if prev != 0 {
        ctxs[n] = mix(mix(1, sh.pos_hash), prev);
        n += 1;
    }
    ctxs[n] = mix(2, sh.pos_hash);
    ctxs[n + 1] = mix(3, sh.p0);
    ctxs[n + 2] = 4;
    n += 3;
    let sym = if dec {
        NOVEL
    } else {
        st.intern.find(sh.cur[col].as_bytes()).unwrap_or(NOVEL)
    };
    let id = match st.ppm.code(c, &ctxs[..n], sym) {
        Some(id) => id,
        None => {
            sh.cat_chars.code_str(c, &[], &sh.cur[col], &mut sh.tmp);
            match st.intern.find(sh.tmp.as_bytes()) {
                Some(i) => i,
                None => st.intern.add(sh.tmp.as_bytes()),
            }
        }
    };
    st.ppm.update(id);
    if dec {
        sh.cur[col].clear();
        sh.cur[col].push_str(std::str::from_utf8(st.intern.get(id)).unwrap_or(""));
    }
}

fn code_cat_group(
    g: &mut CatGroup,
    steps: &mut [StepState],
    plan_steps: &[Step],
    sh: &mut Shared,
    c: &mut Coder,
) {
    let dec = c.is_decoder();
    sh.scratch.clear();
    if !dec {
        for &si in &g.steps {
            let col = plan_steps[si].col;
            put_str(&mut sh.scratch, &sh.cur[col]);
        }
    }
    let sym = if dec {
        NOVEL
    } else {
        g.tuples.find(&sh.scratch).unwrap_or(NOVEL)
    };
    let colh = g.ctx_col.map_or(0, |k| hash_bytes(sh.cur[k].as_bytes()));
    let ctxs = [
        mix(mix(8, sh.pos_hash), colh),
        mix(
            mix(9 + sh.first_in_group as u64, sh.pos_hash),
            sh.prev_cat_id as u64,
        ),
        mix(5, sh.pos_hash),
        mix(6, sh.p0),
        7,
    ];
    let ctxs = if g.ctx_col.is_some() {
        &ctxs[..]
    } else {
        &ctxs[1..]
    };
    let id = match g.ppm.code(c, ctxs, sym) {
        Some(id) => id,
        None => {
            let mut prev = 0u64;
            for &si in &g.steps {
                let col = plan_steps[si].col;
                if let StepState::Cat(st) = &mut steps[si] {
                    code_cat(st, sh, c, col, prev);
                }
                prev = mix(prev | 1, hash_bytes(sh.cur[col].as_bytes()));
            }
            sh.scratch.clear();
            for &si in &g.steps {
                let col = plan_steps[si].col;
                put_str(&mut sh.scratch, &sh.cur[col]);
            }
            match g.tuples.find(&sh.scratch) {
                Some(i) => i,
                None => g.tuples.add(&sh.scratch),
            }
        }
    };
    g.ppm.update(id);
    sh.cat_id = id + 1;
    if dec {
        let b = g.tuples.get(id);
        let mut pos = 0;
        for &si in &g.steps {
            let n = get_varint(b, &mut pos).unwrap_or(0) as usize;
            let end = (pos + n).min(b.len());
            let f = &mut sh.cur[plan_steps[si].col];
            f.clear();
            f.push_str(std::str::from_utf8(&b[pos..end]).unwrap_or(""));
            pos = end;
        }
    }
}

fn runs_of(s: &str, out: &mut Vec<(bool, usize, usize)>) {
    out.clear();
    for (i, ch) in s.char_indices() {
        let k = is_kana(ch);
        match out.last_mut() {
            Some(last) if last.0 == k => last.2 = i + ch.len_utf8(),
            _ => out.push((k, i, i + ch.len_utf8())),
        }
    }
}

fn match_reading(
    surface: &str,
    runs: &[(bool, usize, usize)],
    target: &str,
    groups: &mut Vec<(usize, usize)>,
) -> bool {
    fn rec(
        surface: &str,
        runs: &[(bool, usize, usize)],
        target: &str,
        pos: usize,
        groups: &mut Vec<(usize, usize)>,
    ) -> bool {
        let Some(&(is_k, a, b)) = runs.first() else {
            return pos == target.len();
        };
        if is_k {
            let mut p = pos;
            let mut t = target[pos..].chars();
            for ch in surface[a..b].chars() {
                match t.next() {
                    Some(x) if x == kata(ch) => p += x.len_utf8(),
                    _ => return false,
                }
            }
            return rec(surface, &runs[1..], target, p, groups);
        }
        let mut end = pos;
        for ch in target[pos..].chars() {
            end += ch.len_utf8();
            groups.push((pos, end));
            if rec(surface, &runs[1..], target, end, groups) {
                return true;
            }
            groups.pop();
        }
        false
    }
    groups.clear();
    rec(surface, runs, target, 0, groups)
}

fn common_prefix_len(a: impl Iterator<Item = char>, b: impl Iterator<Item = char>) -> usize {
    a.zip(b).take_while(|(x, y)| x == y).count()
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

const EX_ALIGNED: u32 = 1000;
const EX_DOMAIN: u64 = 1001;

type Cand = (usize, usize, usize, usize);

fn plan_str(st: &StrState, sh: &mut Shared, col: usize, align: bool) -> (u32, Cand) {
    if align {
        runs_of(&sh.cur[0], &mut sh.runs);
        if match_reading(&sh.cur[0], &sh.runs, &sh.cur[col], &mut sh.groups) {
            return (EX_ALIGNED, (0, 0, 0, 0));
        }
    }
    let nsrc = st.srcs.len();
    let target = &sh.cur[col];
    let tlen = target.chars().count();
    let mut best: Option<(usize, usize, usize, usize, usize)> = None;
    let mut consider = |cand: (usize, usize, usize, usize, usize)| {
        if best.is_none_or(|b| (cand.0, cand.1, cand.2, cand.3) < (b.0, b.1, b.2, b.3)) {
            best = Some(cand);
        }
    };
    for (j, &sc) in st.srcs.iter().enumerate() {
        let src = &sh.cur[sc];
        let has_hira = src.chars().any(|ch| kata(ch) != ch);
        let slen = src.chars().count();
        for t in 0..=(has_hira as usize) {
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
    sh: &mut Shared,
    c: &mut Coder,
) {
    let dec = c.is_decoder();
    sh.scratch.clear();
    if !dec {
        for &si in &g.steps {
            if let (StepState::Str(st), Kind::Str { align, .. }) =
                (&mut steps[si], &plan_steps[si].kind)
            {
                let (ex, cand) = plan_str(st, sh, plan_steps[si].col, *align);
                st.group_plan = Some((ex, cand));
                sh.scratch.extend_from_slice(&(ex as u16).to_le_bytes());
            }
        }
    }
    let sym = if dec {
        NOVEL
    } else {
        g.vecs.find(&sh.scratch).unwrap_or(NOVEL)
    };
    let ctxs = [
        mix(
            mix(66 + sh.first_in_group as u64, sh.pos_hash),
            sh.prev_sgroup as u64,
        ),
        mix(60, sh.pos_hash),
        mix(61, sh.p0),
        62,
    ];
    let id = match g.ppm.code(c, &ctxs, sym) {
        Some(id) => id,
        None => {
            sh.scratch.clear();
            for &si in &g.steps {
                if let StepState::Str(st) = &mut steps[si] {
                    let want = st.group_plan.map_or(0, |p| p.0);
                    let ex = code_ex(st, sh, c, want);
                    st.group_plan = st.group_plan.map(|p| (ex, p.1));
                    st.group_exact = ex;
                    sh.scratch.extend_from_slice(&(ex as u16).to_le_bytes());
                }
            }
            match g.vecs.find(&sh.scratch) {
                Some(i) => i,
                None => g.vecs.add(&sh.scratch),
            }
        }
    };
    g.ppm.update(id);
    sh.sgroup = id + 1;
    let b = g.vecs.get(id);
    for (k, &si) in g.steps.iter().enumerate() {
        if let StepState::Str(st) = &mut steps[si] {
            st.group_exact = b
                .get(2 * k..2 * k + 2)
                .map_or(0, |x| u16::from_le_bytes([x[0], x[1]]) as u32);
            if st.group_exact == 0 {
                st.tuple_ppm
                    .prefetch(mix(mix(24, sh.pos_hash), sh.sgroup as u64));
                st.tuple_ppm.prefetch(mix(20, sh.pos_hash));
            }
        }
    }
}

fn code_ex(st: &mut StrState, sh: &mut Shared, c: &mut Coder, want: u32) -> u32 {
    let ctxs = [mix(63, sh.pos_hash), mix(64, sh.p0), 65];
    let ex = match st.ex_ppm.code(c, &ctxs, want) {
        Some(e) => e,
        None => c.uniform(want as u64, EX_DOMAIN) as u32,
    };
    st.ex_ppm.update(ex);
    ex
}

const MAX_RECENT: usize = 8;
pub const FIXED_COLS: usize = 4;
const IRREGULAR_FLAG_LIMIT: u32 = 30000;
const LO_BITS_MAX: u8 = 24;
pub const MAX_STR_STEPS: usize = 200;

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

fn code_str(
    st: &mut StrState,
    sh: &mut Shared,
    c: &mut Coder,
    col: usize,
    align: bool,
    grouped: bool,
) {
    let dec = c.is_decoder();
    let nsrc = st.srcs.len();
    let pending = st.group_plan.take();
    let (ex, cand) = if grouped {
        (st.group_exact, pending.map_or((0, 0, 0, 0), |p| p.1))
    } else {
        let (want, cand) = if dec {
            (0, (0, 0, 0, 0))
        } else {
            plan_str(st, sh, col, align)
        };
        (code_ex(st, sh, c, want), cand)
    };
    sh.shape = mix(sh.shape, ex as u64);
    if ex == EX_ALIGNED && align {
        code_aligned(st, sh, c, col);
    } else if ex != 0 && ex != EX_ALIGNED {
        if dec {
            let (j, t) = (((ex - 1) / 2) as usize, ((ex - 1) % 2) as usize);
            let src = if j < nsrc {
                sh.cur[st.srcs[j]].as_str()
            } else {
                st.recent.get(j - nsrc).map_or("", |x| x.as_str())
            };
            apply_ref(src, t == 1, 0, "", &mut sh.out);
            std::mem::swap(&mut sh.cur[col], &mut sh.out);
        }
    } else {
        code_ref(st, sh, c, col, cand);
    }
    remember_value(st, &sh.cur[col]);
}

fn pair_hash(ch: char, seg: &[char]) -> u64 {
    let mut h = mix(0x5E6, ch as u64);
    for &x in seg {
        h = mix(h, x as u64);
    }
    h
}

fn segment_reading(st: &StrState, chars: &[char], reading: &[char]) -> Vec<(usize, usize)> {
    const MAX_SEG: usize = 8;
    let (n, m) = (chars.len(), reading.len());
    let w = m + 1;
    let inf = f64::MAX / 4.0;
    let mut cost = vec![inf; (n + 1) * w];
    let mut take = vec![0usize; (n + 1) * w];
    cost[0] = 0.0;
    for i in 0..n {
        for k in 0..=m {
            let base = cost[i * w + k];
            if base >= inf {
                continue;
            }
            let longest = if i + 1 == n {
                m - k
            } else {
                MAX_SEG.min(m - k)
            };
            for len in 0..=longest {
                let known = st
                    .known
                    .contains(&pair_hash(chars[i], &reading[k..k + len]));
                let step = if known { 2.0 } else { 5.0 + 6.0 * len as f64 };
                let idx = (i + 1) * w + k + len;
                if base + step < cost[idx] {
                    cost[idx] = base + step;
                    take[idx] = len;
                }
            }
        }
    }
    let mut out = vec![(0, 0); n];
    let mut k = m;
    for i in (0..n).rev() {
        let len = take[(i + 1) * w + k];
        out[i] = (k - len, k);
        k -= len;
    }
    out
}

fn code_segments(st: &mut StrState, c: &mut Coder, run: &str, reading: &str, out: &mut String) {
    let dec = c.is_decoder();
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
        let id = match st.seg_ppm.code(c, &ctxs, sym) {
            Some(id) => id,
            None => {
                let want = piece.clone();
                st.read_chars.code_str(c, &[], &want, &mut piece);
                match st.segs.find(piece.as_bytes()) {
                    Some(i) => i,
                    None => st.segs.add(piece.as_bytes()),
                }
            }
        };
        st.seg_ppm.update(id);
        let text = std::str::from_utf8(st.segs.get(id)).unwrap_or("");
        let _ = seg;
        out.push_str(text);
        let seg_chars: Vec<char> = text.chars().collect();
        st.known.insert(pair_hash(ch, &seg_chars));
    }
}

fn code_aligned(st: &mut StrState, sh: &mut Shared, c: &mut Coder, col: usize) {
    let dec = c.is_decoder();
    runs_of(&sh.cur[0], &mut sh.runs);
    if !dec {
        match_reading(&sh.cur[0], &sh.runs, &sh.cur[col], &mut sh.groups);
    }
    sh.out.clear();
    let mut gi = 0;
    for &(is_k, a, b) in &sh.runs {
        if is_k {
            sh.out.extend(sh.cur[0][a..b].chars().map(kata));
            continue;
        }
        let run = &sh.cur[0][a..b];
        let reading = if dec {
            ""
        } else {
            &sh.cur[col][sh.groups[gi].0..sh.groups[gi].1]
        };
        gi += 1;
        let sym = if dec {
            NOVEL
        } else {
            st.runs.find(reading.as_bytes()).unwrap_or(NOVEL)
        };
        let b0 = c.bits;
        let hit = st
            .run_ppm
            .code(c, &[mix(12, hash_bytes(run.as_bytes()))], sym);
        let b1 = c.bits;
        if hit.is_some() {
            st.align_stats[0] += b1 - b0;
            st.align_stats[1] += 1.0;
        }
        let id = match hit {
            Some(id) => id,
            None => {
                code_segments(st, c, run, reading, &mut sh.tmp);
                st.align_stats[2] += c.bits - b0;
                st.align_stats[3] += 1.0;
                match st.runs.find(sh.tmp.as_bytes()) {
                    Some(i) => i,
                    None => st.runs.add(sh.tmp.as_bytes()),
                }
            }
        };
        st.run_ppm.update(id);
        sh.out
            .push_str(std::str::from_utf8(st.runs.get(id)).unwrap_or(""));
    }
    if dec {
        std::mem::swap(&mut sh.cur[col], &mut sh.out);
    }
}

fn code_ref(st: &mut StrState, sh: &mut Shared, c: &mut Coder, col: usize, cand: Cand) {
    let dec = c.is_decoder();
    let nsrc = st.srcs.len();
    let (bj, bt, btrim, bapp) = cand;
    let make_key = |scratch: &mut Vec<u8>, j: usize, t: usize, trim: usize, app: &str| {
        scratch.clear();
        scratch.push(j as u8);
        scratch.push(t as u8);
        put_varint(scratch, trim as u64);
        scratch.extend_from_slice(app.as_bytes());
    };
    let sym = if dec {
        NOVEL
    } else {
        let app = &sh.cur[col][bapp..];
        make_key(&mut sh.scratch, bj, bt, btrim, app);
        st.tuples.find(&sh.scratch).unwrap_or(NOVEL)
    };
    let ctxs = [
        mix(mix(24, sh.pos_hash), sh.sgroup as u64),
        mix(20, sh.pos_hash),
        mix(21, sh.p0),
        22,
    ];
    let ctxs = if sh.sgroup == 0 {
        &ctxs[1..]
    } else {
        &ctxs[..]
    };
    let id = match st.tuple_ppm.code(c, ctxs, sym) {
        Some(id) => id,
        None => {
            let rsym = (bj * 2 + bt) as u32;
            let r = match st.ref_ppm.code(c, &[mix(23, sh.p0), 24], rsym) {
                Some(r) => r,
                None => c.uniform(rsym as u64, 2 * (nsrc + MAX_RECENT) as u64) as u32,
            };
            st.ref_ppm.update(r);
            let trim = st.trim.code(c, btrim as u64) as usize;
            let app = if dec { "" } else { &sh.cur[col][bapp..] };
            st.chars.code_str(c, &[], app, &mut sh.tmp);
            let (j, t) = ((r / 2) as usize, (r % 2) as usize);
            let tmp = std::mem::take(&mut sh.tmp);
            make_key(&mut sh.scratch, j, t, trim, &tmp);
            sh.tmp = tmp;
            match st.tuples.find(&sh.scratch) {
                Some(i) => i,
                None => st.tuples.add(&sh.scratch),
            }
        }
    };
    st.tuple_ppm.update(id);
    if !dec {
        return;
    }
    let b = st.tuples.get(id);
    let (j, t) = (b[0] as usize, b[1] as usize);
    let mut pos = 2;
    let trim = get_varint(b, &mut pos).unwrap_or(0) as usize;
    let app = std::str::from_utf8(&b[pos..]).unwrap_or("");
    let src = if j < nsrc {
        sh.cur[st.srcs[j]].as_str()
    } else {
        st.recent.get(j - nsrc).map_or("", |x| x.as_str())
    };
    apply_ref(src, t == 1, trim, app, &mut sh.out);
    std::mem::swap(&mut sh.cur[col], &mut sh.out);
}

fn code_num(st: &mut NumState, sh: &mut Shared, c: &mut Coder, col: usize, mode: u8) {
    let dec = c.is_decoder();
    let parsed = if dec {
        None
    } else {
        canonical_i64(&sh.cur[col])
    };
    if sh.row_irr
        && st
            .irregular
            .code(c, !dec && parsed.is_none(), IRREGULAR_FLAG_LIMIT)
    {
        sh.raw_chars.code_str(c, &[], &sh.cur[col], &mut sh.tmp);
        if dec {
            sh.cur[col] = sh.tmp.clone();
        }
        return;
    }
    let reference = if mode == 2 && sh.lid_key != 0 {
        st.refs.get(&sh.lid_key).copied()
    } else {
        None
    };
    if let Some((rcost, rlc)) = reference {
        let ctxs = [
            mix(mix(90, sh.prev_num as u64), rlc as u64),
            mix(mix(91, sh.prev_num as u64), sh.pos_hash),
            mix(92, sh.pos_hash),
            93,
        ];
        let want = parsed.map_or(0, |v| v.wrapping_sub(rcost));
        let sym = match parsed {
            Some(_) => st.d_intern.find(&want.to_le_bytes()).unwrap_or(NOVEL),
            None => NOVEL,
        };
        let id = match st.d_ppm.code(c, &ctxs, sym) {
            Some(id) => id,
            None => {
                let b0 = c.bits;
                let d = unzigzag(st.d_uint.code(c, zigzag(want)));
                st.novel.0 += c.bits - b0;
                st.novel.1 += 1;
                match st.d_intern.find(&d.to_le_bytes()) {
                    Some(i) => i,
                    None => st.d_intern.add(&d.to_le_bytes()),
                }
            }
        };
        st.d_ppm.update(id);
        let d = i64::from_le_bytes(st.d_intern.get(id).try_into().unwrap_or([0; 8]));
        let v = rcost.wrapping_add(d);
        st.refs.insert(sh.lid_key, (v, sh.prev_num));
        note_num(sh, v);
        if dec {
            set_i64(&mut sh.cur[col], v);
        }
        return;
    }
    let ctxs: [u64; 6];
    let n;
    match mode {
        0 => {
            ctxs = [mix(50, sh.p0), 51, 0, 0, 0, 0];
            n = 2;
        }
        2 => {
            ctxs = [
                mix(mix(60, sh.prev_num as u64), sh.pos_hash),
                mix(61, sh.p01),
                62,
                0,
                0,
                0,
            ];
            n = 3;
        }
        1 => {
            let base = mix(sh.pos_sym as u64, sh.cat_id as u64);
            ctxs = [
                mix(mix(70, base), sh.surf_hash),
                mix(71, base),
                mix(72, sh.pos_sym as u64),
                mix(73, sh.p01),
                74,
                0,
            ];
            n = 5;
        }
        _ => {
            let base = mix(mix(sh.pos_sym as u64, sh.cat_id as u64), sh.prev_num as u64);
            ctxs = [
                mix(mix(80, base), sh.surf_hash),
                mix(81, base),
                mix(82, mix(sh.pos_sym as u64, sh.cat_id as u64)),
                mix(83, sh.pos_sym as u64),
                mix(84, sh.p01),
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
    let id = match st.ppm.code(c, ctxs, sym) {
        Some(id) => id,
        None => {
            let b0 = c.bits;
            let v = unzigzag(st.uint.code(c, zigzag(parsed.unwrap_or(0))));
            st.novel.0 += c.bits - b0;
            st.novel.1 += 1;
            match st.intern.find(&v.to_le_bytes()) {
                Some(i) => i,
                None => st.intern.add(&v.to_le_bytes()),
            }
        }
    };
    st.ppm.update(id);
    let v = i64::from_le_bytes(st.intern.get(id).try_into().unwrap_or([0; 8]));
    if mode == 2 && sh.lid_key != 0 {
        st.refs.insert(sh.lid_key, (v, sh.prev_num));
    }
    note_num(sh, v);
    if dec {
        set_i64(&mut sh.cur[col], v);
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

fn code_lid(st: &mut LidState, sh: &mut Shared, c: &mut Coder, col: usize, spec: &LidSpec) {
    let dec = c.is_decoder();
    let (shift, partner, order) = (spec.shift, spec.partner, spec.order);
    let parsed = if dec {
        None
    } else {
        canonical_u64(&sh.cur[col])
    };
    let irregular = sh.row_irr
        && st
            .irregular
            .code(c, !dec && parsed.is_none(), IRREGULAR_FLAG_LIMIT);
    if irregular {
        sh.raw_chars.code_str(c, &[], &sh.cur[col], &mut sh.tmp);
        if dec {
            sh.cur[col].clone_from(&sh.tmp);
        }
    } else {
        let v = parsed.unwrap_or(0);
        let (id, low) = (v >> shift, v & ((1u64 << shift) - 1));
        let mut kh = mix(7, sh.p0);
        for &k in &spec.key {
            kh = mix(kh, hash_bytes(sh.cur[k].as_bytes()));
        }
        let seen = st.seen.get(&kh).copied();
        let mut near = (None, None);
        let want_mode = if seen == Some(id) {
            0
        } else {
            if !dec && let Some(o) = order {
                near = neighbours(&st.order, sh.cur[o].as_bytes());
            }
            let bits = |x: u64| 64 - x.leading_zeros() as u64;
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
        let b0 = c.bits;
        let ctxs = [mix(mix(32, sh.p0), sh.shape), mix(30, sh.p0), 31];
        let mode = match st.mode_ppm.code(c, &ctxs, want_mode) {
            Some(m) => m,
            None => c.uniform(want_mode as u64, 5) as u32,
        };
        st.mode_ppm.update(mode);
        let b1 = c.bits;
        st.stats[0] += b1 - b0;
        st.mode_count[mode as usize % 5] += 1;
        if dec
            && (mode == 1 || mode == 2)
            && let Some(o) = order
        {
            near = neighbours(&st.order, sh.cur[o].as_bytes());
        }
        let new_id = match mode {
            0 => seen.unwrap_or(0),
            1 => {
                let (lo, hi) = near.1.unwrap_or((0, 1));
                lo + c.uniform(id.wrapping_sub(lo), hi - lo)
            }
            2 => {
                let a = near.0.unwrap_or(0) as i64;
                (a + unzigzag(st.delta.code(c, zigzag((id as i64).wrapping_sub(a))))) as u64
            }
            4 => {
                let a = st.last_new as i64;
                (a + unzigzag(st.from_last.code(c, zigzag((id as i64).wrapping_sub(a))))) as u64
            }
            _ => st.direct.code(c, id),
        };
        st.stats[1 + (mode as usize % 5).min(3)] += c.bits - b1;
        let b2 = c.bits;
        if mode != 0 {
            st.last_new = new_id;
        }
        st.seen.insert(kh, new_id);
        if let Some(o) = order {
            let k = sh.cur[o].as_bytes();
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
        let split = shift.min(LO_BITS_MAX) as u32;
        let ctxs = [
            mix(mix(41, sh.pos_hash), sh.shape),
            mix(42, sh.pos_hash),
            mix(43, sh.p0),
            44,
        ];
        let lo = low & ((1u64 << split) - 1);
        let lo = match st.lo_ppm.code(c, &ctxs, lo as u32) {
            Some(x) => x,
            None => c.raw_bits(lo, split) as u32,
        };
        st.lo_ppm.update(lo);
        let mut low = lo as u64;
        if shift as u32 > split {
            let hi_bits = shift as u32 - split;
            let hi = (v & ((1u64 << shift) - 1)) >> split;
            let colh = spec.low_col.map_or(0, |k| hash_bytes(sh.cur[k].as_bytes()));
            let ctxs = [
                mix(mix(50, new_id), colh),
                mix(mix(51, sh.pos_hash), sh.shape),
                mix(52, sh.pos_hash),
                mix(53, sh.p0),
                54,
            ];
            let hi = match st.hi_ppm.code(c, &ctxs, hi as u32) {
                Some(x) => x,
                None => c.raw_bits(hi, hi_bits) as u32,
            };
            st.hi_ppm.update(hi);
            low |= (hi as u64) << split;
        }
        sh.lid_key = mix(mix(77, new_id), low >> 24) | 1;
        st.stats[5] += c.bits - b2;
        if dec {
            set_u64(&mut sh.cur[col], (new_id << shift) | low);
        }
        if let Some(p) = partner {
            let bad = sh.row_irr
                && st.partner_bad.code(
                    c,
                    !dec && canonical_u64(&sh.cur[p]) != Some(new_id),
                    IRREGULAR_FLAG_LIMIT,
                );
            if bad {
                sh.raw_chars.code_str(c, &[], &sh.cur[p], &mut sh.tmp);
                if dec {
                    sh.cur[p].clone_from(&sh.tmp);
                }
            } else if dec {
                set_u64(&mut sh.cur[p], new_id);
            }
        }
        return;
    }
    if let Some(p) = partner {
        sh.raw_chars.code_str(c, &[], &sh.cur[p], &mut sh.tmp);
        if dec {
            sh.cur[p].clone_from(&sh.tmp);
        }
    }
}
