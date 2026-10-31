mod coder;
mod model;
mod plan;
mod ppm;
mod table;
mod text;

use crate::Error;
use crate::wire::{FNV_OFFSET, fnv1a, get_varint, put_varint};
use coder::{Coder, UintModel};
use model::{ColumnAlign, LidReport, Model, NumNovel};
use ppm::{NOVEL, Ppm, mix};
use std::ops::Range;
use std::time::Instant;
use table::{Parsed, parse_text};
use text::CharModel;

pub(crate) use model::Plan;
pub(crate) use table::Lexicon;

const MAGIC: &[u8; 4] = b"MLX1";
const FLAG_NO_FINAL_NEWLINE: u8 = 1;
const FLAG_GROUPED: u8 = 2;
const FLAG_STORED: u8 = 4;
const FLAGS_KNOWN: u8 = 7;
const MAX_KEY_COUNT: usize = 1 << 31;
const PREALLOC_KEY_LIMIT: usize = 1 << 24;
const FLAGS_OFFSET: usize = 4;
const HEADER_PREFIX_LEN: usize = FLAGS_OFFSET + 1;
const DIGEST_BYTES: usize = 8;
const STORED_HEADER_SLACK: usize = 32;
const OVERRUN_MESSAGE: &str = "truncated data";

#[derive(Clone, Default)]
pub(crate) struct EncodeOptions {
    pub plan: Option<Plan>,
    pub report: bool,
    pub verify: bool,
}

#[derive(Clone)]
#[expect(
    dead_code,
    reason = "report fields are for diagnostic tooling and are only written here"
)]
pub(crate) struct StepBits {
    pub col: usize,
    pub kind: String,
    pub bits: f64,
}

#[derive(Default, Clone)]
pub(crate) struct Report {
    pub keys_bits: f64,
    pub pos_bits: f64,
    pub counts_bits: f64,
    pub steps: Vec<StepBits>,
    pub irregular_bits: f64,
    pub lid: Option<LidReport>,
    pub num_novel: Vec<NumNovel>,
    pub align: Vec<ColumnAlign>,
    pub plan_seconds: f64,
    pub code_seconds: f64,
}

struct KeyModel {
    prefix_len: Ppm,
    prefix_len_escape: UintModel,
    first_char_delta: Ppm,
    first_char_delta_escape: UintModel,
    chars: CharModel,
    group_size: Ppm,
    group_size_escape: UintModel,
    prev: Vec<char>,
    cur: Vec<char>,
    last_prefix_len: u32,
}

impl KeyModel {
    fn new() -> Self {
        KeyModel {
            prefix_len: Ppm::default(),
            prefix_len_escape: UintModel::default(),
            first_char_delta: Ppm::default(),
            first_char_delta_escape: UintModel::default(),
            chars: CharModel::new(2),
            group_size: Ppm::default(),
            group_size_escape: UintModel::default(),
            prev: Vec::new(),
            cur: Vec::new(),
            last_prefix_len: 0,
        }
    }

    fn code_key(&mut self, coder: &mut Coder, key: &str, out: &mut String) {
        let dec = coder.is_decoder();
        self.cur.clear();
        let mut shared_len = 0;
        if !dec {
            self.cur.extend(key.chars());
            shared_len = self
                .prev
                .iter()
                .zip(&self.cur)
                .take_while(|(a, b)| a == b)
                .count();
        }
        let prev_len = self.prev.len();
        let ctxs = [
            mix(mix(1, prev_len as u64), u64::from(self.last_prefix_len)),
            mix(1, prev_len as u64),
            2,
        ];
        let shared_len =
            match self
                .prefix_len
                .code(coder, &ctxs, if dec { NOVEL } else { shared_len as u32 })
            {
                Some(shared_len) => shared_len,
                None => saturate_u32(self.prefix_len_escape.code(coder, shared_len as u64)),
            };
        self.prefix_len.update(shared_len);
        let shared_len = (shared_len as usize).min(prev_len);
        self.last_prefix_len = shared_len as u32;
        out.clear();
        out.extend(&self.prev[..shared_len]);
        let mut rest_from = shared_len;
        if shared_len < prev_len {
            let prev_char = self.prev[shared_len] as u32;
            let char_before = if shared_len > 0 {
                self.prev[shared_len - 1] as u64
            } else {
                0
            };
            let delta = if dec {
                0
            } else {
                (self.cur[shared_len] as u32).wrapping_sub(prev_char + 1)
            };
            let ctxs = [
                mix(mix(5, char_before), u64::from(prev_char)),
                mix(6, u64::from(prev_char)),
                7,
            ];
            let delta = match self.first_char_delta.code(coder, &ctxs, delta) {
                Some(delta) => delta,
                None => saturate_u32(self.first_char_delta_escape.code(coder, u64::from(delta))),
            };
            self.first_char_delta.update(delta);
            out.push(
                char::from_u32(prev_char.wrapping_add(1).wrapping_add(delta))
                    .unwrap_or(char::REPLACEMENT_CHARACTER),
            );
            rest_from = shared_len + 1;
        }
        let mut tail: Vec<u32> = out.chars().rev().take(2).map(|ch| ch as u32).collect();
        tail.reverse();
        let rest: String = if dec {
            String::new()
        } else {
            self.cur[rest_from..].iter().collect()
        };
        let mut suffix = String::new();
        self.chars.code_str(coder, &tail, &rest, &mut suffix);
        out.push_str(&suffix);
        self.prev.clear();
        self.prev.extend(out.chars());
    }

    fn code_count(&mut self, coder: &mut Coder, key: &str, count: usize) -> usize {
        let last = key.chars().next_back().map_or(0, |ch| ch as u64);
        let ctxs = [mix(3, last), 4];
        let size = match self.group_size.code(coder, &ctxs, count as u32) {
            Some(size) => size,
            None => saturate_u32(
                self.group_size_escape
                    .code(coder, (count as u64).wrapping_sub(1)),
            )
            .saturating_add(1),
        };
        self.group_size.update(size);
        size as usize
    }
}

fn saturate_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn group_bounds(parsed: &Parsed) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let lex = &parsed.lex;
    let mut i = 0;
    while i < parsed.order.len() {
        let key = lex.field_bytes(parsed.order[i] as usize, 0);
        let mut j = i + 1;
        while j < parsed.order.len() && lex.field_bytes(parsed.order[j] as usize, 0) == key {
            j += 1;
        }
        groups.push(i..j);
        i = j;
    }
    groups
}

pub fn encode_lexicon(csv: &[u8]) -> Vec<u8> {
    let opts = EncodeOptions {
        verify: true,
        ..Default::default()
    };
    encode_with(csv, &opts).0
}

pub(crate) fn encode_with(csv: &[u8], opts: &EncodeOptions) -> (Vec<u8>, Report) {
    let parsed = parse_text(csv);
    let lex = &parsed.lex;
    let groups = group_bounds(&parsed);
    let t0 = Instant::now();
    let plan = match &opts.plan {
        Some(p) => p.clone(),
        None => plan::choose_plan(&parsed, &groups),
    };
    let plan_seconds = t0.elapsed().as_secs_f64();
    let t1 = Instant::now();
    let (digest, text_len) = lex.digest(Some(&parsed.order));

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(header_flags(lex.no_final_newline, parsed.already_grouped));
    put_varint(&mut out, text_len);
    out.extend_from_slice(&digest.to_le_bytes());
    put_varint(&mut out, groups.len() as u64);
    put_varint(&mut out, lex.irregular.len() as u64);
    plan.write(&mut out);

    let mut coder = Coder::encoder(opts.report);
    let mut key_model = KeyModel::new();
    let mut report = Report::default();
    let mut key_buf = String::new();
    for group in &groups {
        let key = lex.field(parsed.order[group.start] as usize, 0);
        key_model.code_key(&mut coder, key, &mut key_buf);
    }
    report.keys_bits = coder.bits;
    let bits_before = coder.bits;
    for group in &groups {
        let key = lex.field(parsed.order[group.start] as usize, 0);
        key_model.code_count(&mut coder, key, group.len());
    }
    report.counts_bits = coder.bits - bits_before;
    if lex.n_cols > 0 {
        let mut model = Model::new(plan);
        let mut fields: Vec<&str> = Vec::with_capacity(lex.n_cols);
        for group in &groups {
            model.start_group();
            for &row in &parsed.order[group.clone()] {
                fields.clear();
                for col in 0..lex.n_cols {
                    fields.push(lex.field(row as usize, col));
                }
                model.code_row(&mut coder, &fields, "");
            }
        }
        report.pos_bits = model.pos_bits;
        report.lid = model.lid_stats();
        report.num_novel = model.num_stats();
        report.align = model.align_stats();
        for (si, s) in model.plan().steps.iter().enumerate() {
            report.steps.push(StepBits {
                col: s.col,
                kind: format!("{:?}", s.kind),
                bits: model.step_bits[si],
            });
        }
    }
    let bits_before = coder.bits;
    let mut irregular_model = CharModel::new(2);
    let mut gaps = UintModel::default();
    let mut line_buf = Vec::new();
    let mut previous_at = 0;
    for (line, &at) in lex.irregular.iter().zip(&lex.irregular_at) {
        gaps.code(&mut coder, u64::from(at - previous_at));
        previous_at = at;
        irregular_model.code_bytes(&mut coder, line, &mut line_buf);
    }
    report.irregular_bits = coder.bits - bits_before;
    out.extend_from_slice(&coder.finish());
    report.plan_seconds = plan_seconds;
    report.code_seconds = t1.elapsed().as_secs_f64();
    if opts.verify && decode_lexicon(&out).is_err() {
        let mut text = Vec::with_capacity(text_len as usize);
        lex.write_text(Some(&parsed.order), |p| text.extend_from_slice(p));
        out = stored(&text, lex.no_final_newline, parsed.already_grouped);
    }
    (out, report)
}

fn stored(text: &[u8], no_final_newline: bool, grouped: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + STORED_HEADER_SLACK);
    out.extend_from_slice(MAGIC);
    out.push(header_flags(no_final_newline, grouped) | FLAG_STORED);
    put_varint(&mut out, text.len() as u64);
    out.extend_from_slice(&fnv1a(FNV_OFFSET, text).to_le_bytes());
    out.extend_from_slice(text);
    out
}

fn header_flags(no_final_newline: bool, grouped: bool) -> u8 {
    let mut flags = 0;
    if no_final_newline {
        flags |= FLAG_NO_FINAL_NEWLINE;
    }
    if grouped {
        flags |= FLAG_GROUPED;
    }
    flags
}

#[inline]
fn ensure_not_overrun(coder: &Coder) -> Result<(), Error> {
    if coder.overrun() {
        return corrupt(OVERRUN_MESSAGE);
    }
    Ok(())
}

fn corrupt<T>(why: &'static str) -> Result<T, Error> {
    Err(Error::Corrupt(why))
}

enum Body {
    Stored,
    Coded {
        n_keys: usize,
        n_irregular: usize,
        plan: Plan,
    },
}

struct Header {
    body: Body,
    no_final_newline: bool,
    text_len: u64,
    digest: u64,
    payload: usize,
}

fn read_header(data: &[u8]) -> Result<Header, Error> {
    if data.len() < HEADER_PREFIX_LEN || &data[..MAGIC.len()] != MAGIC {
        return corrupt("not a lexicon stream");
    }
    let flags = data[FLAGS_OFFSET];
    if flags & !FLAGS_KNOWN != 0 {
        return corrupt("unknown flags");
    }
    let mut pos = HEADER_PREFIX_LEN;
    let Some(text_len) = get_varint(data, &mut pos) else {
        return corrupt("truncated header");
    };
    let Some(digest) = data
        .get(pos..pos + DIGEST_BYTES)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    else {
        return corrupt("truncated header");
    };
    pos += DIGEST_BYTES;
    if flags & FLAG_STORED != 0 {
        return Ok(Header {
            no_final_newline: flags & FLAG_NO_FINAL_NEWLINE != 0,
            body: Body::Stored,
            text_len,
            digest,
            payload: pos,
        });
    }
    let (Some(n_keys), Some(n_irregular)) =
        (get_varint(data, &mut pos), get_varint(data, &mut pos))
    else {
        return corrupt("truncated header");
    };
    if n_keys > text_len || n_irregular > text_len {
        return corrupt("bad line count");
    }
    let Some(plan) = Plan::read(data, &mut pos) else {
        return corrupt("bad plan");
    };
    Ok(Header {
        body: Body::Coded {
            n_keys: n_keys as usize,
            n_irregular: n_irregular as usize,
            plan,
        },
        no_final_newline: flags & FLAG_NO_FINAL_NEWLINE != 0,
        text_len,
        digest,
        payload: pos,
    })
}

fn decode_unchecked(data: &[u8]) -> Result<(Lexicon, Header), Error> {
    let h = read_header(data)?;
    let Body::Coded {
        n_keys,
        n_irregular,
        plan,
    } = &h.body
    else {
        let text = &data[h.payload..];
        if text.len() as u64 != h.text_len || fnv1a(FNV_OFFSET, text) != h.digest {
            return corrupt("digest mismatch");
        }
        let mut lex = parse_text(text).lex;
        lex.no_final_newline = h.no_final_newline;
        return Ok((lex, h));
    };
    let (n_keys, n_irregular) = (*n_keys, *n_irregular);
    let mut coder = Coder::decoder(&data[h.payload..]);
    let n_cols = plan.n_cols;
    let mut lex = Lexicon::new(n_cols);
    lex.no_final_newline = h.no_final_newline;
    if n_keys > MAX_KEY_COUNT || (n_cols == 0 && n_keys > 0) {
        return corrupt("bad key count");
    }
    let mut key_model = KeyModel::new();
    let mut keys: Vec<String> = Vec::with_capacity(n_keys.min(PREALLOC_KEY_LIMIT));
    let mut key_buf = String::new();
    for _ in 0..n_keys {
        key_model.code_key(&mut coder, "", &mut key_buf);
        keys.push(key_buf.clone());
        ensure_not_overrun(&coder)?;
    }
    let mut counts = Vec::with_capacity(keys.len());
    for key in &keys {
        counts.push(key_model.code_count(&mut coder, key, 0));
        ensure_not_overrun(&coder)?;
    }
    let total_rows: u64 = counts.iter().map(|&group_len| group_len as u64).sum();
    if total_rows > h.text_len {
        return corrupt("bad row count");
    }
    if n_cols > 0 {
        if h.text_len > u64::from(u32::MAX)
            || !lex.reserve(total_rows as usize, h.text_len as usize)
        {
            return corrupt("text too large");
        }
        let mut model = Model::new(plan.clone());
        for (key, &group_len) in keys.iter().zip(&counts) {
            model.start_group();
            for _ in 0..group_len {
                model.code_row(&mut coder, &[], key);
                lex.push_row(model.row());
                if coder.overrun() || lex.data_len() as u64 > h.text_len {
                    return corrupt(OVERRUN_MESSAGE);
                }
            }
        }
    }
    let mut irregular_model = CharModel::new(2);
    let mut gaps = UintModel::default();
    let rows = lex.rows() as u64;
    let mut at = 0u64;
    for _ in 0..n_irregular {
        at = at.saturating_add(gaps.code(&mut coder, 0));
        if at > rows {
            return corrupt("an irregular line lies past the last row");
        }
        let mut line = Vec::new();
        irregular_model.code_bytes(&mut coder, &[], &mut line);
        lex.irregular_at.push(at as u32);
        lex.irregular.push(line);
        ensure_not_overrun(&coder)?;
    }
    Ok((lex, h))
}

pub fn decode_lexicon(data: &[u8]) -> Result<Vec<u8>, Error> {
    let (lex, h) = decode_unchecked(data)?;
    let text = lex.to_text();
    if fnv1a(FNV_OFFSET, &text) != h.digest || text.len() as u64 != h.text_len {
        return corrupt("digest mismatch");
    }
    Ok(text)
}
