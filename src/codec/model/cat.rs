use super::row::{RowState, put_str, read_str};
use super::spec::{Kind, Step};
use super::state::StepState;
use crate::codec::coder::Coder;
use crate::codec::hints::HintReader;
use crate::codec::ppm::{Interner, NOVEL, Ppm, PpmHint, hash_bytes, mix};

pub(super) const NO_PREVIOUS: u64 = 0;
const MAX_CAT_CONTEXTS: usize = 4;
const GROUP_CTX_COUNT: usize = 5;

pub(super) struct CatState {
    ppm: Ppm,
    intern: Interner,
}

impl CatState {
    pub(super) fn new(hints: &mut HintReader) -> CatState {
        CatState {
            ppm: hints.ppm(),
            intern: hints.interner(),
        }
    }

    pub(super) fn sizes(&self, out: &mut Vec<u32>) {
        self.intern.sizes(out);
    }

    pub(super) fn hints(&self, out: &mut Vec<PpmHint>) {
        out.push(self.ppm.hint());
    }
}

pub(super) struct CatGroup {
    ppm: Ppm,
    tuples: Interner,
    pub steps: Vec<usize>,
    ctx_col: Option<usize>,
}

impl CatGroup {
    pub(super) fn hints(&self, out: &mut Vec<PpmHint>) {
        out.push(self.ppm.hint());
    }

    pub(super) fn sizes(&self, out: &mut Vec<u32>) {
        self.tuples.sizes(out);
    }

    pub(super) fn new(
        plan_steps: &[Step],
        ctx_col: Option<usize>,
        hints: &mut HintReader,
    ) -> CatGroup {
        CatGroup {
            ppm: hints.ppm(),
            tuples: hints.interner(),
            ctx_col,
            steps: plan_steps
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind == Kind::Cat)
                .map(|(i, _)| i)
                .collect(),
        }
    }

    #[inline]
    pub(super) fn prefetch(&self, row: &RowState) {
        self.ppm.prefetch(mix(
            mix(9 + u64::from(row.first_in_group), row.pos_hash),
            u64::from(row.prev_cat_group_id),
        ));
        self.ppm.prefetch(mix(5, row.pos_hash));
    }

    #[inline]
    fn put_key(&self, plan_steps: &[Step], row: &mut RowState) {
        row.key_buf.clear();
        for &si in &self.steps {
            let col = plan_steps[si].col;
            put_str(&mut row.key_buf, &row.fields[col]);
        }
    }

    #[inline]
    fn contexts(&self, row: &RowState) -> ([u64; GROUP_CTX_COUNT], usize) {
        let colh = self
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
        (ctxs, usize::from(self.ctx_col.is_none()))
    }

    fn code_members(
        &mut self,
        steps: &mut [StepState],
        plan_steps: &[Step],
        row: &mut RowState,
        coder: &mut Coder,
    ) -> u32 {
        let mut prev = NO_PREVIOUS;
        for &si in &self.steps {
            let col = plan_steps[si].col;
            if let StepState::Cat(st) = &mut steps[si] {
                code_cat(st, row, coder, col, prev);
            }
            prev = mix(prev | 1, hash_bytes(row.fields[col].as_bytes()));
        }
        self.put_key(plan_steps, row);
        self.tuples.find_or_add(&row.key_buf)
    }

    #[inline]
    fn load_members(&self, plan_steps: &[Step], row: &mut RowState, id: u32) {
        let tuple = self.tuples.get(id);
        let mut pos = 0;
        for &si in &self.steps {
            read_str(tuple, &mut pos, &mut row.fields[plan_steps[si].col]);
        }
    }
}

#[inline]
fn cat_contexts(row: &RowState, prev: u64) -> ([u64; MAX_CAT_CONTEXTS], usize) {
    let mut ctxs = [0u64; MAX_CAT_CONTEXTS];
    let mut ctx_count = 0;
    if prev != NO_PREVIOUS {
        ctxs[ctx_count] = mix(mix(1, row.pos_hash), prev);
        ctx_count += 1;
    }
    ctxs[ctx_count] = mix(2, row.pos_hash);
    ctxs[ctx_count + 1] = mix(3, row.pos_first_hash);
    ctxs[ctx_count + 2] = 4;
    (ctxs, ctx_count + 3)
}

pub(super) fn code_cat(
    st: &mut CatState,
    row: &mut RowState,
    coder: &mut Coder,
    col: usize,
    prev: u64,
) {
    let dec = coder.is_decoder();
    let (ctxs, ctx_count) = cat_contexts(row, prev);
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

pub(super) fn code_cat_group(
    g: &mut CatGroup,
    steps: &mut [StepState],
    plan_steps: &[Step],
    row: &mut RowState,
    coder: &mut Coder,
) {
    let dec = coder.is_decoder();
    if dec {
        row.key_buf.clear();
    } else {
        g.put_key(plan_steps, row);
    }
    let sym = if dec {
        NOVEL
    } else {
        g.tuples.find(&row.key_buf).unwrap_or(NOVEL)
    };
    let (ctxs, skip) = g.contexts(row);
    let id = if let Some(id) = g.ppm.code(coder, &ctxs[skip..], sym) {
        id
    } else {
        g.code_members(steps, plan_steps, row, coder)
    };
    g.ppm.update(id);
    row.cat_group_id = id + 1;
    if dec {
        g.load_members(plan_steps, row, id);
    }
}
