use super::align::{Run, Span};
use crate::codec::coder::Coder;
use crate::codec::hints::HintReader;
use crate::codec::ppm::{NOVEL, PpmHint, hash_bytes, mix};
use crate::codec::text::CharModel;
use crate::wire::{get_varint, put_varint};

const RAW_CHAR_ORDER: usize = 2;
const CAT_CHAR_ORDER: usize = 1;

pub(super) struct RowState {
    pub fields: Vec<String>,
    pub coded_text: String,
    pub key_buf: Vec<u8>,
    pub raw_chars: CharModel,
    pub cat_chars: CharModel,
    pub pos_hash: u64,
    pub pos_first_hash: u64,
    pub pos_first_two_hash: u64,
    pub pos_sym: u32,
    pub prev_pos: u32,
    pub last_char: u32,
    pub first_in_group: bool,
    pub prev_num: i64,
    pub num_seen: bool,
    pub lid_ref_key: u64,
    pub cat_group_id: u32,
    pub surf_hash: u64,
    pub cat_pos_ctx: Option<[u64; 3]>,
    pub cat_group_ctx: Option<[u64; 2]>,
    pub str_group_ctx: Option<u64>,
    pub ref_ctx: Option<[u64; 3]>,
    pub str_shape: u64,
    pub str_group_id: u32,
    pub prev_str_group_id: u32,
    pub prev_cat_group_id: u32,
    pub row_irregular: bool,
    pub rebuilt: String,
    pub script_runs: Vec<Run>,
    pub reading_spans: Vec<Span>,
}

impl RowState {
    pub(super) fn hints(&self, out: &mut Vec<PpmHint>) {
        self.raw_chars.hints(out);
        self.cat_chars.hints(out);
    }

    pub(super) fn new(n_cols: usize, hints: &mut HintReader) -> RowState {
        RowState {
            fields: vec![String::new(); n_cols],
            coded_text: String::new(),
            key_buf: Vec::new(),
            raw_chars: CharModel::new(RAW_CHAR_ORDER, hints),
            cat_chars: CharModel::new(CAT_CHAR_ORDER, hints),
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
            cat_pos_ctx: None,
            cat_group_ctx: None,
            str_group_ctx: None,
            ref_ctx: None,
            str_shape: 0,
            str_group_id: 0,
            prev_str_group_id: 0,
            prev_cat_group_id: 0,
            row_irregular: true,
            rebuilt: String::new(),
            script_runs: Vec::new(),
            reading_spans: Vec::new(),
        }
    }

    #[inline]
    pub(super) fn load_fields(&mut self, coder: &Coder, fields: &[&str], surface: &str) {
        if coder.is_decoder() {
            self.fields[0].clear();
            self.fields[0].push_str(surface);
        } else {
            for (i, f) in fields.iter().enumerate() {
                self.fields[i].clear();
                self.fields[i].push_str(f);
            }
        }
        self.last_char = self.fields[0].chars().next_back().map_or(0, |ch| ch as u32);
        self.prev_num = 0;
        self.num_seen = false;
        self.lid_ref_key = 0;
    }

    #[inline]
    pub(super) fn finish_row(&mut self) {
        self.prev_pos = self.pos_sym;
        self.first_in_group = false;
        self.prev_str_group_id = self.str_group_id;
        self.prev_cat_group_id = self.cat_group_id;
    }

    #[inline]
    pub(super) fn note_num(&mut self, v: i64) {
        if !self.num_seen {
            self.num_seen = true;
            self.prev_num = v;
        }
    }

    #[inline]
    pub(super) fn code_raw_field(&mut self, coder: &mut Coder, col: usize) {
        self.raw_chars
            .code_str(coder, &[], &self.fields[col], &mut self.coded_text);
        if coder.is_decoder() {
            self.fields[col].clone_from(&self.coded_text);
        }
    }
}

#[inline]
pub(super) fn read_str(bytes: &[u8], pos: &mut usize, dest: &mut String) {
    let len = get_varint(bytes, pos).unwrap_or(0) as usize;
    let end = pos.saturating_add(len).min(bytes.len());
    dest.clear();
    dest.push_str(std::str::from_utf8(&bytes[*pos..end]).unwrap_or(""));
    *pos = end;
}

#[inline]
pub(super) fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

const CAT_POS_SALT: u64 = 1;
const CAT_POS_ORDER2_SALT: u64 = 2;
const CAT_POS_FIRST_SALT: u64 = 3;
const CAT_GROUP_FIRST_SALT: u64 = 9;
const CAT_GROUP_POS_SALT: u64 = 5;
const STR_GROUP_FIRST_SALT: u64 = 66;
const REF_GROUP_SALT: u64 = 24;
const REF_POS_SALT: u64 = 20;
const REF_FIRST_SALT: u64 = 21;

impl RowState {
    #[inline]
    pub(super) fn cat_pos_ctx(&mut self) -> [u64; 3] {
        let (pos, first) = (self.pos_hash, self.pos_first_hash);
        *self.cat_pos_ctx.get_or_insert_with(|| {
            [
                mix(CAT_POS_SALT, pos),
                mix(CAT_POS_ORDER2_SALT, pos),
                mix(CAT_POS_FIRST_SALT, first),
            ]
        })
    }

    #[inline]
    pub(super) fn cat_group_ctx(&mut self) -> [u64; 2] {
        let (pos, first_in_group, prev) =
            (self.pos_hash, self.first_in_group, self.prev_cat_group_id);
        *self.cat_group_ctx.get_or_insert_with(|| {
            [
                mix(
                    mix(CAT_GROUP_FIRST_SALT + u64::from(first_in_group), pos),
                    u64::from(prev),
                ),
                mix(CAT_GROUP_POS_SALT, pos),
            ]
        })
    }

    #[inline]
    pub(super) fn str_group_ctx(&mut self) -> u64 {
        let (pos, first_in_group, prev) =
            (self.pos_hash, self.first_in_group, self.prev_str_group_id);
        *self.str_group_ctx.get_or_insert_with(|| {
            mix(
                mix(STR_GROUP_FIRST_SALT + u64::from(first_in_group), pos),
                u64::from(prev),
            )
        })
    }

    #[inline]
    pub(super) fn ref_ctx(&mut self) -> [u64; 3] {
        let (pos, first, group) = (self.pos_hash, self.pos_first_hash, self.str_group_id);
        *self.ref_ctx.get_or_insert_with(|| {
            [
                mix(mix(REF_GROUP_SALT, pos), u64::from(group)),
                mix(REF_POS_SALT, pos),
                mix(REF_FIRST_SALT, first),
            ]
        })
    }
}

impl RowState {
    #[inline]
    pub(super) fn surface_hash(&mut self) -> u64 {
        if self.surf_hash == 0 {
            self.surf_hash = hash_bytes(self.fields[0].as_bytes());
        }
        self.surf_hash
    }
}
