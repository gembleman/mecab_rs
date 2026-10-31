mod coder;
mod model;
mod plan;
mod ppm;
mod table;
mod text;

use crate::Error;
use crate::wire::{FNV_OFFSET, fnv1a, get_varint, put_varint};
use coder::{Coder, UintModel};
use model::Model;
use ppm::{NOVEL, Ppm, mix};
use std::time::Instant;
use table::{Parsed, parse_text};
use text::CharModel;

pub use model::Plan;
pub use table::Lexicon;

const MAGIC: &[u8; 4] = b"MLX1";
const FLAG_NO_FINAL_NEWLINE: u8 = 1;
const FLAG_STORED: u8 = 4;
const FLAGS_KNOWN: u8 = 7;
const MAX_KEY_COUNT: usize = 1 << 31;
const PREALLOC_KEY_LIMIT: usize = 1 << 24;

#[derive(Clone, Default)]
pub struct EncodeOptions {
    pub plan: Option<Plan>,
    pub report: bool,
    pub verify: bool,
}

#[derive(Default, Clone)]
pub struct Report {
    pub keys_bits: f64,
    pub pos_bits: f64,
    pub counts_bits: f64,
    pub steps: Vec<(usize, String, f64)>,
    pub irregular_bits: f64,
    pub lid: Option<([f64; 6], [u64; 5])>,
    pub num_novel: Vec<(f64, u64)>,
    pub align: Vec<(usize, [f64; 4])>,
    pub plan_seconds: f64,
    pub code_seconds: f64,
}

struct KeyModel {
    lcp: Ppm,
    lcp_uint: UintModel,
    first: Ppm,
    first_uint: UintModel,
    chars: CharModel,
    cnt: Ppm,
    cnt_uint: UintModel,
    prev: Vec<char>,
    cur: Vec<char>,
    last_l: u32,
}

impl KeyModel {
    fn new() -> Self {
        KeyModel {
            lcp: Ppm::default(),
            lcp_uint: UintModel::default(),
            first: Ppm::default(),
            first_uint: UintModel::default(),
            chars: CharModel::new(2),
            cnt: Ppm::default(),
            cnt_uint: UintModel::default(),
            prev: Vec::new(),
            cur: Vec::new(),
            last_l: 0,
        }
    }

    fn code_key(&mut self, c: &mut Coder, key: &str, out: &mut String) {
        let dec = c.is_decoder();
        self.cur.clear();
        let mut l = 0;
        if !dec {
            self.cur.extend(key.chars());
            l = self
                .prev
                .iter()
                .zip(&self.cur)
                .take_while(|(a, b)| a == b)
                .count();
        }
        let prev_len = self.prev.len();
        let ctxs = [
            mix(mix(1, prev_len as u64), self.last_l as u64),
            mix(1, prev_len as u64),
            2,
        ];
        let l = match self.lcp.code(c, &ctxs, if dec { NOVEL } else { l as u32 }) {
            Some(l) => l,
            None => self.lcp_uint.code(c, l as u64) as u32,
        };
        self.lcp.update(l);
        let l = (l as usize).min(prev_len);
        self.last_l = l as u32;
        out.clear();
        out.extend(&self.prev[..l]);
        let mut rest_from = l;
        if l < prev_len {
            let pc = self.prev[l] as u32;
            let before = if l > 0 { self.prev[l - 1] as u64 } else { 0 };
            let d = if dec {
                0
            } else {
                (self.cur[l] as u32).wrapping_sub(pc + 1)
            };
            let ctxs = [mix(mix(5, before), pc as u64), mix(6, pc as u64), 7];
            let d = match self.first.code(c, &ctxs, d) {
                Some(d) => d,
                None => self.first_uint.code(c, d as u64) as u32,
            };
            self.first.update(d);
            out.push(
                char::from_u32(pc.wrapping_add(1).wrapping_add(d))
                    .unwrap_or(char::REPLACEMENT_CHARACTER),
            );
            rest_from = l + 1;
        }
        let mut tail: Vec<u32> = out.chars().rev().take(2).map(|ch| ch as u32).collect();
        tail.reverse();
        let rest: String = if dec {
            String::new()
        } else {
            self.cur[rest_from..].iter().collect()
        };
        let mut suffix = String::new();
        self.chars.code_str(c, &tail, &rest, &mut suffix);
        out.push_str(&suffix);
        self.prev.clear();
        self.prev.extend(out.chars());
    }

    fn code_count(&mut self, c: &mut Coder, key: &str, count: usize) -> usize {
        let last = key.chars().next_back().map_or(0, |ch| ch as u64);
        let ctxs = [mix(3, last), 4];
        let n = match self.cnt.code(c, &ctxs, count as u32) {
            Some(n) => n,
            None => self.cnt_uint.code(c, (count as u64).wrapping_sub(1)) as u32 + 1,
        };
        self.cnt.update(n);
        n as usize
    }
}

fn group_bounds(parsed: &Parsed) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let lex = &parsed.lex;
    let mut i = 0;
    while i < parsed.order.len() {
        let key = lex.field_bytes(parsed.order[i] as usize, 0);
        let mut j = i + 1;
        while j < parsed.order.len() && lex.field_bytes(parsed.order[j] as usize, 0) == key {
            j += 1;
        }
        groups.push((i, j));
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

pub fn encode_with(csv: &[u8], opts: &EncodeOptions) -> (Vec<u8>, Report) {
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
    out.push(lex.no_final_newline as u8 | (parsed.already_grouped as u8) << 1);
    put_varint(&mut out, text_len);
    out.extend_from_slice(&digest.to_le_bytes());
    put_varint(&mut out, groups.len() as u64);
    put_varint(&mut out, lex.irregular.len() as u64);
    plan.write(&mut out);

    let mut c = Coder::encoder(opts.report);
    let mut km = KeyModel::new();
    let mut report = Report {
        ..Default::default()
    };
    let mut buf = String::new();
    for &(a, _) in &groups {
        let key = lex.field(parsed.order[a] as usize, 0);
        km.code_key(&mut c, key, &mut buf);
    }
    report.keys_bits = c.bits;
    let before = c.bits;
    for &(a, b) in &groups {
        let key = lex.field(parsed.order[a] as usize, 0);
        km.code_count(&mut c, key, b - a);
    }
    report.counts_bits = c.bits - before;
    if lex.n_cols > 0 {
        let mut model = Model::new(plan);
        let mut fields: Vec<&str> = Vec::with_capacity(lex.n_cols);
        for &(a, b) in &groups {
            model.start_group();
            for &r in &parsed.order[a..b] {
                fields.clear();
                for col in 0..lex.n_cols {
                    fields.push(lex.field(r as usize, col));
                }
                model.code_row(&mut c, &fields, "");
            }
        }
        report.pos_bits = model.pos_bits;
        report.lid = model.lid_stats();
        report.num_novel = model.num_stats();
        report.align = model.align_stats();
        for (si, s) in model.plan().steps.iter().enumerate() {
            report
                .steps
                .push((s.col, format!("{:?}", s.kind), model.step_bits[si]));
        }
    }
    let before = c.bits;
    let mut irr = CharModel::new(2);
    let mut gaps = UintModel::default();
    let mut tmp = Vec::new();
    let mut previous_at = 0;
    for (l, &at) in lex.irregular.iter().zip(&lex.irregular_at) {
        gaps.code(&mut c, u64::from(at - previous_at));
        previous_at = at;
        irr.code_bytes(&mut c, l, &mut tmp);
    }
    report.irregular_bits = c.bits - before;
    out.extend_from_slice(&c.finish());
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
    let mut out = Vec::with_capacity(text.len() + 32);
    out.extend_from_slice(MAGIC);
    out.push(no_final_newline as u8 | (grouped as u8) << 1 | FLAG_STORED);
    put_varint(&mut out, text.len() as u64);
    out.extend_from_slice(&fnv1a(FNV_OFFSET, text).to_le_bytes());
    out.extend_from_slice(text);
    out
}

fn corrupt<T>(why: &'static str) -> Result<T, Error> {
    Err(Error::Corrupt(why))
}

struct Header {
    stored: bool,
    no_final_newline: bool,
    text_len: u64,
    digest: u64,
    n_keys: usize,
    n_irregular: usize,
    plan: Plan,
    payload: usize,
}

fn read_header(data: &[u8]) -> Result<Header, Error> {
    if data.len() < 5 || &data[..4] != MAGIC {
        return corrupt("not a lexicon stream");
    }
    let flags = data[4];
    if flags & !FLAGS_KNOWN != 0 {
        return corrupt("unknown flags");
    }
    let mut pos = 5;
    let Some(text_len) = get_varint(data, &mut pos) else {
        return corrupt("truncated header");
    };
    let Some(digest) = data
        .get(pos..pos + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    else {
        return corrupt("truncated header");
    };
    pos += 8;
    if flags & FLAG_STORED != 0 {
        return Ok(Header {
            no_final_newline: flags & FLAG_NO_FINAL_NEWLINE != 0,
            stored: true,
            text_len,
            digest,
            n_keys: 0,
            n_irregular: 0,
            plan: Plan::default(),
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
        stored: false,
        no_final_newline: flags & FLAG_NO_FINAL_NEWLINE != 0,
        text_len,
        digest,
        n_keys: n_keys as usize,
        n_irregular: n_irregular as usize,
        plan,
        payload: pos,
    })
}

fn decode_unchecked(data: &[u8]) -> Result<(Lexicon, Header), Error> {
    let h = read_header(data)?;
    if h.stored {
        let text = &data[h.payload..];
        if text.len() as u64 != h.text_len || fnv1a(FNV_OFFSET, text) != h.digest {
            return corrupt("digest mismatch");
        }
        let mut lex = parse_text(text).lex;
        lex.no_final_newline = h.no_final_newline;
        return Ok((lex, h));
    }
    let mut c = Coder::decoder(&data[h.payload..]);
    let n_cols = h.plan.n_cols;
    let mut lex = Lexicon::new(n_cols);
    lex.no_final_newline = h.no_final_newline;
    if h.n_keys > MAX_KEY_COUNT || (n_cols == 0 && h.n_keys > 0) {
        return corrupt("bad key count");
    }
    let mut km = KeyModel::new();
    let mut keys: Vec<String> = Vec::with_capacity(h.n_keys.min(PREALLOC_KEY_LIMIT));
    let mut buf = String::new();
    for _ in 0..h.n_keys {
        km.code_key(&mut c, "", &mut buf);
        keys.push(buf.clone());
        if c.overrun() {
            return corrupt("truncated data");
        }
    }
    let mut counts = Vec::with_capacity(keys.len());
    for k in &keys {
        counts.push(km.code_count(&mut c, k, 0));
        if c.overrun() {
            return corrupt("truncated data");
        }
    }
    let total_rows: u64 = counts.iter().map(|&n| n as u64).sum();
    if total_rows > h.text_len {
        return corrupt("bad row count");
    }
    if n_cols > 0 {
        if h.text_len > u32::MAX as u64 || !lex.reserve(total_rows as usize, h.text_len as usize) {
            return corrupt("text too large");
        }
        let mut model = Model::new(h.plan.clone());
        for (k, &n) in keys.iter().zip(&counts) {
            model.start_group();
            for _ in 0..n {
                model.code_row(&mut c, &[], k);
                lex.push_row(model.row());
                if c.overrun() || lex.data_len() as u64 > h.text_len {
                    return corrupt("truncated data");
                }
            }
        }
    }
    let mut irr = CharModel::new(2);
    let mut gaps = UintModel::default();
    let rows = lex.rows() as u64;
    let mut at = 0u64;
    for _ in 0..h.n_irregular {
        at = at.saturating_add(gaps.code(&mut c, 0));
        if at > rows {
            return corrupt("an irregular line lies past the last row");
        }
        let mut line = Vec::new();
        irr.code_bytes(&mut c, &[], &mut line);
        lex.irregular_at.push(at as u32);
        lex.irregular.push(line);
        if c.overrun() {
            return corrupt("truncated data");
        }
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
