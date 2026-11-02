use super::digits::{canonical_u64, set_u64};
use super::num::IRREGULAR_FLAG_LIMIT;
use super::row::RowState;
use super::spec::LidSpec;
use crate::codec::coder::{Coder, Flag, UintModel};
use crate::codec::hints::HintReader;
use crate::codec::ppm::{Ppm, PpmHint, hash_bytes, mix};
use crate::hash::IdMap;
use crate::wire::{unzigzag, zigzag};
use std::collections::BTreeMap;
use std::ops::Bound;

const LID_MODE_COUNT: usize = 5;
const LO_BITS_MAX: u8 = 24;

type OrderRanges = BTreeMap<Vec<u8>, (u64, u64)>;
type Neighbours = (Option<u64>, Option<(u64, u64)>);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LidMode {
    Seen,
    Range,
    OrderDelta,
    Direct,
    LastNewDelta,
}

impl LidMode {
    fn from_symbol(symbol: u32) -> LidMode {
        match symbol {
            0 => LidMode::Seen,
            1 => LidMode::Range,
            2 => LidMode::OrderDelta,
            4 => LidMode::LastNewDelta,
            _ => LidMode::Direct,
        }
    }

    fn to_symbol(self) -> u32 {
        match self {
            LidMode::Seen => 0,
            LidMode::Range => 1,
            LidMode::OrderDelta => 2,
            LidMode::Direct => 3,
            LidMode::LastNewDelta => 4,
        }
    }
}

pub(super) struct LidState {
    seen: IdMap<u64, u64>,
    order: OrderRanges,
    mode_ppm: Ppm,
    delta: UintModel,
    direct: UintModel,
    from_last: UintModel,
    lo_ppm: Ppm,
    hi_ppm: Ppm,
    irregular: Flag,
    partner_bad: Flag,
    last_new: u64,
}

impl LidState {
    pub(super) fn new(hints: &mut HintReader) -> LidState {
        LidState {
            seen: hints.map(),
            order: BTreeMap::new(),
            mode_ppm: hints.ppm(),
            delta: UintModel::default(),
            direct: UintModel::default(),
            from_last: UintModel::default(),
            lo_ppm: hints.ppm(),
            hi_ppm: hints.ppm(),
            irregular: Flag::default(),
            partner_bad: Flag::default(),
            last_new: 0,
        }
    }

    pub(super) fn hints(&self, out: &mut Vec<PpmHint>) {
        out.push(self.mode_ppm.hint());
        out.push(self.lo_ppm.hint());
        out.push(self.hi_ppm.hint());
    }

    pub(super) fn sizes(&self, out: &mut Vec<u32>) {
        out.push(self.seen.len() as u32);
    }

    fn choose_mode(&self, near: Neighbours, id: u64, seen: Option<u64>) -> LidMode {
        if seen == Some(id) {
            return LidMode::Seen;
        }
        let bits = |x: u64| 64 - u64::from(x.leading_zeros());
        if near.0.is_some() {
            LidMode::OrderDelta
        } else if near.1.is_some_and(|(lo, hi)| lo <= id && id < hi) {
            LidMode::Range
        } else if 2 * bits(id.abs_diff(self.last_new)) + 1 < bits(id) + 2 {
            LidMode::LastNewDelta
        } else {
            LidMode::Direct
        }
    }

    fn code_mode(&mut self, row: &RowState, coder: &mut Coder, want: LidMode) -> LidMode {
        let ctxs = [
            mix(mix(32, row.pos_first_hash), row.str_shape),
            mix(30, row.pos_first_hash),
            31,
        ];
        let mode_symbol = match self.mode_ppm.code(coder, &ctxs, want.to_symbol()) {
            Some(m) => m,
            None => coder.uniform(u64::from(want.to_symbol()), LID_MODE_COUNT as u64) as u32,
        };
        self.mode_ppm.update(mode_symbol);
        LidMode::from_symbol(mode_symbol)
    }

    fn code_new_id(
        &mut self,
        coder: &mut Coder,
        mode: LidMode,
        id: u64,
        seen: Option<u64>,
        near: Neighbours,
    ) -> u64 {
        match mode {
            LidMode::Seen => seen.unwrap_or(0),
            LidMode::Range => {
                let (lo, hi) = near.1.unwrap_or((0, 1));
                lo + coder.uniform(id.wrapping_sub(lo), hi - lo)
            }
            LidMode::OrderDelta => {
                let a = near.0.unwrap_or(0) as i64;
                a.wrapping_add(unzigzag(
                    self.delta.code(coder, zigzag((id as i64).wrapping_sub(a))),
                )) as u64
            }
            LidMode::LastNewDelta => {
                let a = self.last_new as i64;
                a.wrapping_add(unzigzag(
                    self.from_last
                        .code(coder, zigzag((id as i64).wrapping_sub(a))),
                )) as u64
            }
            LidMode::Direct => self.direct.code(coder, id),
        }
    }

    fn record_new_id(
        &mut self,
        row: &RowState,
        spec: &LidSpec,
        mode: LidMode,
        key_hash: u64,
        new_id: u64,
    ) {
        if mode != LidMode::Seen {
            self.last_new = new_id;
        }
        self.seen.insert(key_hash, new_id);
        if let Some(o) = spec.order {
            let k = row.fields[o].as_bytes();
            match self.order.get_mut(k) {
                Some(e) => {
                    e.0 = e.0.min(new_id);
                    e.1 = e.1.max(new_id);
                }
                None => {
                    self.order.insert(k.to_vec(), (new_id, new_id));
                }
            }
        }
    }

    fn code_lo(&mut self, row: &RowState, coder: &mut Coder, low: u64, split: u32) -> u64 {
        let ctxs = [
            mix(mix(41, row.pos_hash), row.str_shape),
            mix(42, row.pos_hash),
            mix(43, row.pos_first_hash),
            44,
        ];
        let lo = low & ((1u64 << split) - 1);
        let lo = match self.lo_ppm.code(coder, &ctxs, lo as u32) {
            Some(x) => x,
            None => coder.raw_bits(lo, split) as u32,
        };
        self.lo_ppm.update(lo);
        u64::from(lo)
    }

    fn code_hi(
        &mut self,
        row: &RowState,
        coder: &mut Coder,
        spec: &LidSpec,
        new_id: u64,
        v: u64,
        split: u32,
    ) -> u64 {
        let hi_bits = u32::from(spec.shift) - split;
        let hi = (v & ((1u64 << spec.shift) - 1)) >> split;
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
        let hi = match self.hi_ppm.code(coder, &ctxs, hi as u32) {
            Some(x) => x,
            None => coder.raw_bits(hi, hi_bits) as u32,
        };
        self.hi_ppm.update(hi);
        u64::from(hi)
    }

    fn code_low_bits(
        &mut self,
        row: &RowState,
        coder: &mut Coder,
        spec: &LidSpec,
        new_id: u64,
        v: u64,
    ) -> u64 {
        let shift = spec.shift;
        let split = u32::from(shift.min(LO_BITS_MAX));
        let mut low = self.code_lo(row, coder, v & ((1u64 << shift) - 1), split);
        if u32::from(shift) > split {
            low |= self.code_hi(row, coder, spec, new_id, v, split) << split;
        }
        low
    }

    fn code_partner(&mut self, row: &mut RowState, coder: &mut Coder, p: usize, new_id: u64) {
        let dec = coder.is_decoder();
        let bad = row.row_irregular
            && self.partner_bad.code(
                coder,
                !dec && canonical_u64(&row.fields[p]) != Some(new_id),
                IRREGULAR_FLAG_LIMIT,
            );
        if bad {
            row.code_raw_field(coder, p);
        } else if dec {
            set_u64(&mut row.fields[p], new_id);
        }
    }
}

fn key_hash(row: &RowState, spec: &LidSpec) -> u64 {
    let mut kh = mix(7, row.pos_first_hash);
    for &k in &spec.key {
        kh = mix(kh, hash_bytes(row.fields[k].as_bytes()));
    }
    kh
}

fn neighbours(order: &OrderRanges, key: &[u8]) -> Neighbours {
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

fn encoder_neighbours(st: &LidState, row: &RowState, spec: &LidSpec) -> Neighbours {
    spec.order.map_or((None, None), |o| {
        neighbours(&st.order, row.fields[o].as_bytes())
    })
}

fn code_lid_regular(
    st: &mut LidState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    spec: &LidSpec,
    v: u64,
) {
    let dec = coder.is_decoder();
    let shift = spec.shift;
    let id = v >> shift;
    let kh = key_hash(row, spec);
    let seen = st.seen.get(&kh).copied();
    let mut near = (None, None);
    if seen != Some(id) && !dec {
        near = encoder_neighbours(st, row, spec);
    }
    let want_mode = st.choose_mode(near, id, seen);
    let mode = st.code_mode(row, coder, want_mode);
    if dec && matches!(mode, LidMode::Range | LidMode::OrderDelta) {
        near = encoder_neighbours(st, row, spec);
    }
    let new_id = st.code_new_id(coder, mode, id, seen, near);
    st.record_new_id(row, spec, mode, kh, new_id);
    let low = st.code_low_bits(row, coder, spec, new_id, v);
    row.lid_ref_key = mix(mix(77, new_id), low >> LO_BITS_MAX) | 1;
    if dec {
        set_u64(&mut row.fields[col], (new_id << shift) | low);
    }
    if let Some(p) = spec.partner {
        st.code_partner(row, coder, p, new_id);
    }
}

pub(super) fn code_lid(
    st: &mut LidState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    spec: &LidSpec,
) {
    let dec = coder.is_decoder();
    let parsed = if dec {
        None
    } else {
        canonical_u64(&row.fields[col])
    };
    let irregular = row.row_irregular
        && st
            .irregular
            .code(coder, !dec && parsed.is_none(), IRREGULAR_FLAG_LIMIT);
    if !irregular {
        code_lid_regular(st, row, coder, col, spec, parsed.unwrap_or(0));
        return;
    }
    row.code_raw_field(coder, col);
    if let Some(p) = spec.partner {
        row.code_raw_field(coder, p);
    }
}
