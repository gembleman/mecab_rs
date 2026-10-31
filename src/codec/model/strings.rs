use super::align::code_aligned;
use super::row::RowState;
use super::spec::MAX_RECENT;
use super::strings_plan::plan_str;
use crate::codec::coder::{Coder, UintModel};
use crate::codec::ppm::{Interner, NOVEL, Ppm, mix};
use crate::codec::text::{CharModel, kata};
use crate::wire::{get_varint, put_varint};
use std::collections::HashSet;

pub(super) const EX_ALIGNED: u32 = 1000;
const EX_DOMAIN: u64 = 1001;
const EX_REF_OFFSET: u32 = 1;
const STR_CHAR_ORDER: usize = 2;
const SOURCE_SYMBOL_STRIDE: usize = 2;
const KATAKANA_BYTE_FLAG: u8 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SourceRef {
    pub index: usize,
    pub to_katakana: bool,
}

impl SourceRef {
    fn to_symbol(self) -> u32 {
        (self.index * SOURCE_SYMBOL_STRIDE + usize::from(self.to_katakana)) as u32
    }

    fn from_symbol(symbol: u32) -> SourceRef {
        SourceRef {
            index: (symbol / 2) as usize,
            to_katakana: symbol % 2 == 1,
        }
    }

    pub(super) fn to_ex(self) -> u32 {
        EX_REF_OFFSET + self.to_symbol()
    }

    fn from_ex(ex: u32) -> SourceRef {
        SourceRef::from_symbol(ex - EX_REF_OFFSET)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RefChoice {
    pub source: SourceRef,
    pub trim: usize,
    pub append_from: usize,
}

#[derive(Clone, Copy)]
pub(super) struct GroupPlan {
    pub exact: u32,
    pub choice: RefChoice,
}

pub(super) struct StrState {
    pub srcs: Vec<usize>,
    pub tuple_ppm: Ppm,
    tuples: Interner,
    ref_ppm: Ppm,
    trim: UintModel,
    chars: CharModel,
    ex_ppm: Ppm,
    pub run_ppm: Ppm,
    pub run_readings: Interner,
    pub read_chars: CharModel,
    pub recent: Vec<String>,
    limit: usize,
    pub group_plan: Option<GroupPlan>,
    pub group_exact: u32,
    pub seg_ppm: Ppm,
    pub segs: Interner,
    pub known: HashSet<u64>,
}

impl StrState {
    pub(super) fn new(recent_limit: u8, srcs: Vec<usize>) -> StrState {
        StrState {
            limit: recent_limit as usize,
            srcs,
            tuple_ppm: Ppm::default(),
            tuples: Interner::default(),
            ref_ppm: Ppm::default(),
            trim: UintModel::default(),
            chars: CharModel::new(STR_CHAR_ORDER),
            ex_ppm: Ppm::default(),
            run_ppm: Ppm::default(),
            run_readings: Interner::default(),
            read_chars: CharModel::new(STR_CHAR_ORDER),
            recent: Vec::new(),
            group_plan: None,
            group_exact: 0,
            seg_ppm: Ppm::default(),
            segs: Interner::default(),
            known: HashSet::default(),
        }
    }

    pub(super) fn code_ex(&mut self, row: &RowState, coder: &mut Coder, want: u32) -> u32 {
        let ctxs = [mix(63, row.pos_hash), mix(64, row.pos_first_hash), 65];
        let ex = match self.ex_ppm.code(coder, &ctxs, want) {
            Some(e) => e,
            None => coder.uniform(u64::from(want), EX_DOMAIN) as u32,
        };
        self.ex_ppm.update(ex);
        ex
    }

    fn remember_value(&mut self, v: &str) {
        remember(&mut self.recent, self.limit, v);
    }
}

#[inline]
pub(super) fn resolve_source<'a>(st: &'a StrState, fields: &'a [String], index: usize) -> &'a str {
    let source_count = st.srcs.len();
    if index < source_count {
        fields[st.srcs[index]].as_str()
    } else {
        st.recent
            .get(index - source_count)
            .map_or("", |x| x.as_str())
    }
}

#[inline]
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

#[inline]
fn decode_exact_ref(st: &StrState, row: &mut RowState, col: usize, ex: u32) {
    let source = SourceRef::from_ex(ex);
    let src = resolve_source(st, &row.fields, source.index);
    apply_ref(src, source.to_katakana, 0, "", &mut row.rebuilt);
    std::mem::swap(&mut row.fields[col], &mut row.rebuilt);
}

pub(super) fn code_str(
    st: &mut StrState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    align: bool,
    grouped: bool,
) {
    let dec = coder.is_decoder();
    let pending = st.group_plan.take();
    let (ex, choice) = if grouped {
        (
            st.group_exact,
            pending.map_or(RefChoice::default(), |p| p.choice),
        )
    } else {
        let (want, choice) = if dec {
            (0, RefChoice::default())
        } else {
            plan_str(st, row, col, align)
        };
        (st.code_ex(row, coder, want), choice)
    };
    row.str_shape = mix(row.str_shape, u64::from(ex));
    if ex == EX_ALIGNED && align {
        code_aligned(st, row, coder, col);
    } else if ex != 0 && ex != EX_ALIGNED {
        if dec {
            decode_exact_ref(st, row, col, ex);
        }
    } else {
        code_ref(st, row, coder, col, choice);
    }
    st.remember_value(&row.fields[col]);
}

fn put_ref_key(key: &mut Vec<u8>, source: SourceRef, trim: usize, app: &str) {
    key.clear();
    key.push(source.index as u8);
    key.push(u8::from(source.to_katakana));
    put_varint(key, trim as u64);
    key.extend_from_slice(app.as_bytes());
}

#[inline]
fn ref_contexts(row: &RowState) -> ([u64; 4], usize) {
    let ctxs = [
        mix(mix(24, row.pos_hash), u64::from(row.str_group_id)),
        mix(20, row.pos_hash),
        mix(21, row.pos_first_hash),
        22,
    ];
    (ctxs, usize::from(row.str_group_id == 0))
}

fn code_novel_ref(
    st: &mut StrState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    choice: RefChoice,
) -> u32 {
    let dec = coder.is_decoder();
    let rsym = choice.source.to_symbol();
    let r = if let Some(r) = st
        .ref_ppm
        .code(coder, &[mix(23, row.pos_first_hash), 24], rsym)
    {
        r
    } else {
        let domain = (SOURCE_SYMBOL_STRIDE * (st.srcs.len() + MAX_RECENT)) as u64;
        coder.uniform(u64::from(rsym), domain) as u32
    };
    st.ref_ppm.update(r);
    let trim = st.trim.code(coder, choice.trim as u64) as usize;
    let app = if dec {
        ""
    } else {
        &row.fields[col][choice.append_from..]
    };
    st.chars.code_str(coder, &[], app, &mut row.coded_text);
    let tmp = std::mem::take(&mut row.coded_text);
    put_ref_key(&mut row.key_buf, SourceRef::from_symbol(r), trim, &tmp);
    row.coded_text = tmp;
    st.tuples.find_or_add(&row.key_buf)
}

#[inline]
fn rebuild_from_ref(st: &StrState, row: &mut RowState, col: usize, id: u32) {
    let key = st.tuples.get(id);
    let source = SourceRef {
        index: key[0] as usize,
        to_katakana: key[1] == KATAKANA_BYTE_FLAG,
    };
    let mut pos = 2;
    let trim = get_varint(key, &mut pos).unwrap_or(0) as usize;
    let app = std::str::from_utf8(&key[pos..]).unwrap_or("");
    let src = resolve_source(st, &row.fields, source.index);
    apply_ref(src, source.to_katakana, trim, app, &mut row.rebuilt);
    std::mem::swap(&mut row.fields[col], &mut row.rebuilt);
}

fn code_ref(
    st: &mut StrState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    choice: RefChoice,
) {
    let dec = coder.is_decoder();
    let sym = if dec {
        NOVEL
    } else {
        let app = &row.fields[col][choice.append_from..];
        put_ref_key(&mut row.key_buf, choice.source, choice.trim, app);
        st.tuples.find(&row.key_buf).unwrap_or(NOVEL)
    };
    let (ctxs, skip) = ref_contexts(row);
    let id = if let Some(id) = st.tuple_ppm.code(coder, &ctxs[skip..], sym) {
        id
    } else {
        code_novel_ref(st, row, coder, col, choice)
    };
    st.tuple_ppm.update(id);
    if dec {
        rebuild_from_ref(st, row, col, id);
    }
}
