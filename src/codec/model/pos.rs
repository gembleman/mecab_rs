use super::row::{RowState, put_str, read_str};
use super::spec::FIXED_COLS;
use crate::codec::coder::Coder;
use crate::codec::hints::HintReader;
use crate::codec::ppm::{Interner, NOVEL, Ppm, PpmHint, hash_bytes, mix};
use crate::codec::text::CharModel;

const COMP_CHAR_ORDER: usize = 1;
const TUPLE_FIRST_SALT: u64 = 100;
const TUPLE_CHAR_SALT: u64 = 101;
const TUPLE_ORDER0_CTX: u64 = 102;
const COMP_CTX_SALT: u64 = 110;
const COMP_ORDER0_SALT: u64 = 120;

#[derive(Clone, Copy, Default)]
struct PosHashes {
    first: u64,
    first_two: u64,
    all: u64,
}

pub(super) struct PosModel {
    ppm: Ppm,
    intern: Interner,
    comp_ppm: Vec<Ppm>,
    comp_intern: Vec<Interner>,
    comp_chars: CharModel,
    hashes: Vec<PosHashes>,
    cols: usize,
}

impl PosModel {
    pub(super) fn new(cols: usize, hints: &mut HintReader) -> PosModel {
        PosModel {
            ppm: hints.ppm(),
            intern: Interner::default(),
            comp_ppm: (0..cols).map(|_| hints.ppm()).collect(),
            comp_intern: (0..cols).map(|_| Interner::default()).collect(),
            comp_chars: CharModel::new(COMP_CHAR_ORDER, hints),
            hashes: Vec::new(),
            cols,
        }
    }

    pub(super) fn hints(&self, out: &mut Vec<PpmHint>) {
        out.push(self.ppm.hint());
        out.extend(self.comp_ppm.iter().map(Ppm::hint));
        self.comp_chars.hints(out);
    }

    #[inline]
    fn put_encoder_key(&self, row: &mut RowState) {
        row.key_buf.clear();
        for i in 0..self.cols {
            put_str(&mut row.key_buf, &row.fields[FIXED_COLS + i]);
        }
    }

    #[inline]
    fn tuple_contexts(row: &RowState) -> [u64; 3] {
        [
            mix(
                mix(
                    TUPLE_FIRST_SALT + u64::from(row.first_in_group),
                    u64::from(row.prev_pos),
                ),
                u64::from(row.last_char),
            ),
            mix(TUPLE_CHAR_SALT, u64::from(row.last_char)),
            TUPLE_ORDER0_CTX,
        ]
    }

    fn code_component(
        &mut self,
        row: &mut RowState,
        coder: &mut Coder,
        i: usize,
        h: u64,
    ) -> String {
        let dec = coder.is_decoder();
        let ctxs = [
            mix(COMP_CTX_SALT + i as u64, h),
            COMP_ORDER0_SALT + i as u64,
        ];
        let fsym = if dec {
            NOVEL
        } else {
            self.comp_intern[i]
                .find(row.fields[FIXED_COLS + i].as_bytes())
                .unwrap_or(NOVEL)
        };
        let fid = if let Some(f) = self.comp_ppm[i].code(coder, &ctxs, fsym) {
            f
        } else {
            self.comp_chars
                .code_str(coder, &[], &row.fields[FIXED_COLS + i], &mut row.coded_text);
            self.comp_intern[i].find_or_add(row.coded_text.as_bytes())
        };
        self.comp_ppm[i].update(fid);
        std::str::from_utf8(self.comp_intern[i].get(fid))
            .unwrap_or("")
            .to_string()
    }

    fn code_novel_tuple(&mut self, row: &mut RowState, coder: &mut Coder) -> u32 {
        let mut h = 0u64;
        row.key_buf.clear();
        for i in 0..self.cols {
            let f = self.code_component(row, coder, i, h);
            h = mix(h, hash_bytes(f.as_bytes()));
            put_str(&mut row.key_buf, &f);
            row.fields[FIXED_COLS + i] = f;
        }
        self.intern.find_or_add(&row.key_buf)
    }

    #[inline]
    fn code_tuple_id(&mut self, row: &mut RowState, coder: &mut Coder, sym: u32) -> u32 {
        let ctxs = Self::tuple_contexts(row);
        let id = if let Some(id) = self.ppm.code(coder, &ctxs, sym) {
            id
        } else {
            self.code_novel_tuple(row, coder)
        };
        self.ppm.update(id);
        id
    }

    #[inline]
    fn load_tuple_fields(&self, row: &mut RowState, id: u32) {
        let tuple = self.intern.get(id);
        let mut pos = 0;
        for i in 0..self.cols {
            read_str(tuple, &mut pos, &mut row.fields[FIXED_COLS + i]);
        }
    }

    fn compute_hashes(&self, row: &RowState) -> PosHashes {
        let mut hashes = PosHashes::default();
        let mut h = 0u64;
        for i in 0..self.cols {
            h = mix(h, hash_bytes(row.fields[FIXED_COLS + i].as_bytes()));
            if i == 0 {
                hashes.first = h;
            }
            if i == 1 {
                hashes.first_two = h;
            }
        }
        if self.cols == 1 {
            hashes.first_two = hashes.first;
        }
        hashes.all = h | 1;
        hashes
    }

    #[inline]
    fn apply_hashes(&mut self, row: &mut RowState, id: u32) {
        if let Some(&hashes) = self.hashes.get(id as usize).filter(|x| x.all != 0) {
            row.pos_first_hash = hashes.first;
            row.pos_first_two_hash = hashes.first_two;
            row.pos_hash = hashes.all;
            return;
        }
        let hashes = self.compute_hashes(row);
        row.pos_first_hash = hashes.first;
        row.pos_first_two_hash = hashes.first_two;
        row.pos_hash = hashes.all;
        if self.hashes.len() <= id as usize {
            self.hashes.resize(id as usize + 1, PosHashes::default());
        }
        self.hashes[id as usize] = hashes;
    }

    pub(super) fn code(&mut self, row: &mut RowState, coder: &mut Coder, trial: bool) {
        let dec = coder.is_decoder();
        row.key_buf.clear();
        if !dec {
            self.put_encoder_key(row);
        }
        let sym = if dec {
            NOVEL
        } else {
            self.intern.find(&row.key_buf).unwrap_or(NOVEL)
        };
        let id = if trial {
            self.intern.find_or_add(&row.key_buf)
        } else {
            self.code_tuple_id(row, coder, sym)
        };
        row.pos_sym = id;
        if dec {
            self.load_tuple_fields(row, id);
        }
        self.apply_hashes(row, id);
    }
}
