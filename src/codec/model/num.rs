use super::digits::{canonical_i64, set_i64};
use super::row::RowState;
use super::spec::NumMode;
use crate::codec::coder::{Coder, Flag, UintModel};
use crate::codec::ppm::{Interner, NOVEL, Ppm, mix};
use crate::hash::IdMap;
use crate::wire::{unzigzag, zigzag};

pub(super) const IRREGULAR_FLAG_LIMIT: u32 = 30000;
const MAX_NUM_CONTEXTS: usize = 6;
const I64_BYTES: usize = 8;

pub(super) struct NumState {
    ppm: Ppm,
    intern: Interner,
    uint: UintModel,
    irregular: Flag,
    d_ppm: Ppm,
    d_intern: Interner,
    d_uint: UintModel,
    refs: IdMap<u64, (i64, i64)>,
}

impl NumState {
    pub(super) fn new() -> NumState {
        NumState {
            ppm: Ppm::default(),
            intern: Interner::default(),
            uint: UintModel::default(),
            irregular: Flag::default(),
            d_ppm: Ppm::default(),
            d_intern: Interner::default(),
            d_uint: UintModel::default(),
            refs: IdMap::default(),
        }
    }
}

#[inline]
fn read_i64(bytes: &[u8]) -> i64 {
    i64::from_le_bytes(bytes.try_into().unwrap_or([0; I64_BYTES]))
}

#[inline]
fn num_contexts(row: &RowState, mode: NumMode) -> ([u64; MAX_NUM_CONTEXTS], usize) {
    match mode {
        NumMode::Plain => ([mix(50, row.pos_first_hash), 51, 0, 0, 0, 0], 2),
        NumMode::LidLinked => (
            [
                mix(mix(60, row.prev_num as u64), row.pos_hash),
                mix(61, row.pos_first_two_hash),
                62,
                0,
                0,
                0,
            ],
            3,
        ),
        NumMode::Surface => {
            let base = mix(u64::from(row.pos_sym), u64::from(row.cat_group_id));
            (
                [
                    mix(mix(70, base), row.surf_hash),
                    mix(71, base),
                    mix(72, u64::from(row.pos_sym)),
                    mix(73, row.pos_first_two_hash),
                    74,
                    0,
                ],
                5,
            )
        }
        NumMode::SurfacePrevious => {
            let base = mix(
                mix(u64::from(row.pos_sym), u64::from(row.cat_group_id)),
                row.prev_num as u64,
            );
            (
                [
                    mix(mix(80, base), row.surf_hash),
                    mix(81, base),
                    mix(82, mix(u64::from(row.pos_sym), u64::from(row.cat_group_id))),
                    mix(83, u64::from(row.pos_sym)),
                    mix(84, row.pos_first_two_hash),
                    85,
                ],
                6,
            )
        }
    }
}

fn code_num_delta(
    st: &mut NumState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    parsed: Option<i64>,
    reference: (i64, i64),
) {
    let dec = coder.is_decoder();
    let (rcost, rlc) = reference;
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
        let d = unzigzag(st.d_uint.code(coder, zigzag(want)));
        st.d_intern.find_or_add(&d.to_le_bytes())
    };
    st.d_ppm.update(id);
    let d = read_i64(st.d_intern.get(id));
    let v = rcost.wrapping_add(d);
    st.refs.insert(row.lid_ref_key, (v, row.prev_num));
    row.note_num(v);
    if dec {
        set_i64(&mut row.fields[col], v);
    }
}

fn code_num_value(
    st: &mut NumState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    parsed: Option<i64>,
    mode: NumMode,
) {
    let dec = coder.is_decoder();
    let (ctxs, n) = num_contexts(row, mode);
    let skip = usize::from(mode.uses_surface() && st.ppm.distinct(ctxs[1]) < 2);
    let ctxs = &ctxs[skip..n];
    let sym = match parsed {
        Some(v) => st.intern.find(&v.to_le_bytes()).unwrap_or(NOVEL),
        None => NOVEL,
    };
    let id = if let Some(id) = st.ppm.code(coder, ctxs, sym) {
        id
    } else {
        let v = unzigzag(st.uint.code(coder, zigzag(parsed.unwrap_or(0))));
        st.intern.find_or_add(&v.to_le_bytes())
    };
    st.ppm.update(id);
    let v = read_i64(st.intern.get(id));
    if mode == NumMode::LidLinked && row.lid_ref_key != 0 {
        st.refs.insert(row.lid_ref_key, (v, row.prev_num));
    }
    row.note_num(v);
    if dec {
        set_i64(&mut row.fields[col], v);
    }
}

pub(super) fn code_num(
    st: &mut NumState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    mode: NumMode,
) {
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
        row.code_raw_field(coder, col);
        return;
    }
    let reference = if mode == NumMode::LidLinked && row.lid_ref_key != 0 {
        st.refs.get(&row.lid_ref_key).copied()
    } else {
        None
    };
    match reference {
        Some(reference) => code_num_delta(st, row, coder, col, parsed, reference),
        None => code_num_value(st, row, coder, col, parsed, mode),
    }
}
