use crate::wire::{get_varint, put_varint};

const TAG_CAT: u8 = 0;
const TAG_STR: u8 = 1;
const TAG_STR_ALIGNED: u8 = 2;
const TAG_NUM: u8 = 3;
const TAG_LID: u8 = 4;
const MAX_COLS: usize = 1 << 20;
pub(crate) const MAX_RECENT: usize = 8;
pub(crate) const FIXED_COLS: usize = 4;
pub(crate) const MAX_STR_STEPS: usize = 200;
pub(crate) const MAX_LID_SHIFT: u8 = 54;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct LidSpec {
    pub shift: u8,
    pub partner: Option<usize>,
    pub order: Option<usize>,
    pub key: Vec<usize>,
    pub low_col: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum NumMode {
    Plain = 0,
    Surface = 1,
    LidLinked = 2,
    SurfacePrevious = 3,
}

impl TryFrom<u8> for NumMode {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, ()> {
        match value {
            0 => Ok(NumMode::Plain),
            1 => Ok(NumMode::Surface),
            2 => Ok(NumMode::LidLinked),
            3 => Ok(NumMode::SurfacePrevious),
            _ => Err(()),
        }
    }
}

impl NumMode {
    pub(super) fn uses_surface(self) -> bool {
        matches!(self, NumMode::Surface | NumMode::SurfacePrevious)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Cat,
    Str { align: bool, recent: u8 },
    Lid(LidSpec),
    Num { mode: NumMode },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub col: usize,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Plan {
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

fn write_lid_spec(out: &mut Vec<u8>, spec: &LidSpec) {
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

fn write_step(out: &mut Vec<u8>, step: &Step) {
    put_varint(out, step.col as u64);
    match &step.kind {
        Kind::Cat => out.push(TAG_CAT),
        Kind::Str { align, recent } => {
            out.push(TAG_STR + u8::from(*align));
            out.push(*recent);
        }
        Kind::Num { mode } => {
            out.push(TAG_NUM);
            out.push(*mode as u8);
        }
        Kind::Lid(spec) => write_lid_spec(out, spec),
    }
}

fn read_lid_spec(bytes: &[u8], pos: &mut usize, n_cols: usize) -> Option<LidSpec> {
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
    Some(LidSpec {
        shift,
        partner,
        order,
        key,
        low_col,
    })
}

fn read_str_kind(bytes: &[u8], pos: &mut usize, tag: u8) -> Option<Kind> {
    let recent = read_u8(bytes, pos)?;
    if recent as usize > MAX_RECENT {
        return None;
    }
    Some(Kind::Str {
        align: tag == TAG_STR_ALIGNED,
        recent,
    })
}

fn read_kind(bytes: &[u8], pos: &mut usize, n_cols: usize) -> Option<Kind> {
    let tag = read_u8(bytes, pos)?;
    match tag {
        TAG_CAT => Some(Kind::Cat),
        TAG_STR | TAG_STR_ALIGNED => read_str_kind(bytes, pos, tag),
        TAG_NUM => {
            let mode = NumMode::try_from(read_u8(bytes, pos)?).ok()?;
            Some(Kind::Num { mode })
        }
        TAG_LID => read_lid_spec(bytes, pos, n_cols).map(Kind::Lid),
        _ => None,
    }
}

fn read_step(bytes: &[u8], pos: &mut usize, n_cols: usize) -> Option<Step> {
    let col = read_usize(bytes, pos)?;
    let kind = read_kind(bytes, pos, n_cols)?;
    (col < n_cols).then_some(Step { col, kind })
}

impl Plan {
    pub(crate) fn probe(n_cols: usize, pos_cols: usize) -> Plan {
        Plan {
            n_cols,
            pos_cols,
            ..Default::default()
        }
    }

    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        put_varint(out, self.n_cols as u64);
        put_varint(out, self.pos_cols as u64);
        put_opt(out, self.cat_ctx);
        put_varint(out, self.steps.len() as u64);
        for step in &self.steps {
            write_step(out, step);
        }
    }

    pub(crate) fn read(bytes: &[u8], pos: &mut usize) -> Option<Plan> {
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
            steps.push(read_step(bytes, pos, n_cols)?);
        }
        let plan = Plan {
            n_cols,
            pos_cols,
            cat_ctx,
            steps,
        };
        plan.valid().then_some(plan)
    }

    fn initially_coded(&self) -> Vec<bool> {
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
        coded
    }

    fn step_refs_coded(&self, step: &Step, coded: &[bool]) -> bool {
        let refs: Vec<usize> = match &step.kind {
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
            Kind::Str { align: true, .. } if step.col == 0 => return false,
            Kind::Cat => self.cat_ctx.into_iter().collect(),
            _ => Vec::new(),
        };
        !(refs.iter().any(|&r| r >= self.n_cols || !coded[r]) || coded[step.col])
    }

    fn claim_partner(&self, step: &Step, coded: &mut [bool]) -> bool {
        let Kind::Lid(LidSpec {
            partner: Some(p), ..
        }) = &step.kind
        else {
            return true;
        };
        if *p >= self.n_cols || coded[*p] {
            return false;
        }
        coded[*p] = true;
        true
    }

    pub(crate) fn valid(&self) -> bool {
        let mut coded = self.initially_coded();
        let mut strings = 0;
        for s in &self.steps {
            if !self.step_refs_coded(s, &coded) {
                return false;
            }
            if matches!(s.kind, Kind::Str { .. }) {
                strings += 1;
                if strings > MAX_STR_STEPS {
                    return false;
                }
            }
            coded[s.col] = true;
            if !self.claim_partner(s, &mut coded) {
                return false;
            }
        }
        true
    }
}
