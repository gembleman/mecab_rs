mod coder;
mod header;
mod key_model;
mod model;
mod plan_choice;
mod ppm;
mod table;
mod text;

use crate::Error;
use crate::wire::{FNV_OFFSET, fnv1a};
use coder::{Coder, UintModel};
use header::{
    Body, CodedHeader, Header, OVERRUN_MESSAGE, corrupt, ensure_not_overrun, header_flags,
    read_header, stored, write_coded_header,
};
use key_model::KeyModel;
use model::{ColumnAlign, LidReport, Model, NumNovel};
use std::ops::Range;
use std::time::Instant;
use table::{Parsed, parse_text};
use text::CharModel;

pub(crate) use model::Plan;
pub(crate) use table::Lexicon;

const MAX_KEY_COUNT: usize = 1 << 31;
const PREALLOC_KEY_LIMIT: usize = 1 << 24;
const IRREGULAR_CHAR_ORDER: usize = 2;

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

struct EncodeInput<'a> {
    parsed: &'a Parsed,
    groups: &'a [Range<usize>],
}

impl EncodeInput<'_> {
    fn group_key(&self, group: &Range<usize>) -> &str {
        self.parsed
            .lex
            .field(self.parsed.order[group.start] as usize, 0)
    }
}

fn encode_keys(
    input: &EncodeInput,
    key_model: &mut KeyModel,
    coder: &mut Coder,
    report: &mut Report,
) {
    let mut key_buf = String::new();
    for group in input.groups {
        key_model.code_key(coder, input.group_key(group), &mut key_buf);
    }
    report.keys_bits = coder.bits;
}

fn encode_counts(
    input: &EncodeInput,
    key_model: &mut KeyModel,
    coder: &mut Coder,
    report: &mut Report,
) {
    let bits_before = coder.bits;
    for group in input.groups {
        key_model.code_count(coder, input.group_key(group), group.len());
    }
    report.counts_bits = coder.bits - bits_before;
}

fn encode_rows(input: &EncodeInput, plan: Plan, coder: &mut Coder, report: &mut Report) {
    let lex = &input.parsed.lex;
    if lex.n_cols == 0 {
        return;
    }
    let mut model = Model::new(plan);
    let mut fields: Vec<&str> = Vec::with_capacity(lex.n_cols);
    for group in input.groups {
        model.start_group();
        for &row in &input.parsed.order[group.clone()] {
            fields.clear();
            for col in 0..lex.n_cols {
                fields.push(lex.field(row as usize, col));
            }
            model.code_row(coder, &fields, "");
        }
    }
    record_model_report(&model, report);
}

fn record_model_report(model: &Model, report: &mut Report) {
    report.pos_bits = model.pos_bits();
    report.lid = model.lid_stats();
    report.num_novel = model.num_stats();
    report.align = model.align_stats();
    for (si, s) in model.plan().steps.iter().enumerate() {
        report.steps.push(StepBits {
            col: s.col,
            kind: format!("{:?}", s.kind),
            bits: model.step_bits()[si],
        });
    }
}

fn encode_irregular(lex: &Lexicon, coder: &mut Coder, report: &mut Report) {
    let bits_before = coder.bits;
    let mut irregular_model = CharModel::new(IRREGULAR_CHAR_ORDER);
    let mut gaps = UintModel::default();
    let mut line_buf = Vec::new();
    let mut previous_at = 0;
    for (line, &at) in lex.irregular.iter().zip(&lex.irregular_at) {
        gaps.code(coder, u64::from(at - previous_at));
        previous_at = at;
        irregular_model.code_bytes(coder, line, &mut line_buf);
    }
    report.irregular_bits = coder.bits - bits_before;
}

fn verified_or_stored(out: Vec<u8>, parsed: &Parsed, text_len: u64) -> Vec<u8> {
    if decode_lexicon(&out).is_ok() {
        return out;
    }
    let lex = &parsed.lex;
    let mut text = Vec::with_capacity(text_len as usize);
    lex.write_text(Some(&parsed.order), |p| text.extend_from_slice(p));
    stored(&text, lex.no_final_newline, parsed.already_grouped)
}

pub(crate) fn encode_with(csv: &[u8], opts: &EncodeOptions) -> (Vec<u8>, Report) {
    let parsed = parse_text(csv);
    let lex = &parsed.lex;
    let groups = group_bounds(&parsed);
    let input = EncodeInput {
        parsed: &parsed,
        groups: &groups,
    };
    let t0 = Instant::now();
    let plan = match &opts.plan {
        Some(p) => p.clone(),
        None => plan_choice::choose_plan(&parsed, &groups),
    };
    let plan_seconds = t0.elapsed().as_secs_f64();
    let t1 = Instant::now();
    let (digest, text_len) = lex.digest(Some(&parsed.order));

    let mut out = Vec::new();
    write_coded_header(
        &mut out,
        &CodedHeader {
            flags: header_flags(lex.no_final_newline, parsed.already_grouped),
            text_len,
            digest,
            n_keys: groups.len(),
            n_irregular: lex.irregular.len(),
            plan: &plan,
        },
    );

    let mut coder = Coder::encoder(opts.report);
    let mut key_model = KeyModel::new();
    let mut report = Report::default();
    encode_keys(&input, &mut key_model, &mut coder, &mut report);
    encode_counts(&input, &mut key_model, &mut coder, &mut report);
    encode_rows(&input, plan, &mut coder, &mut report);
    encode_irregular(lex, &mut coder, &mut report);
    out.extend_from_slice(&coder.finish());
    report.plan_seconds = plan_seconds;
    report.code_seconds = t1.elapsed().as_secs_f64();
    if opts.verify {
        out = verified_or_stored(out, &parsed, text_len);
    }
    (out, report)
}

fn decode_stored(data: &[u8], h: &Header) -> Result<Lexicon, Error> {
    let text = &data[h.payload..];
    if text.len() as u64 != h.text_len || fnv1a(FNV_OFFSET, text) != h.digest {
        return corrupt("digest mismatch");
    }
    let mut lex = parse_text(text).lex;
    lex.no_final_newline = h.no_final_newline;
    Ok(lex)
}

fn decode_keys(
    coder: &mut Coder,
    key_model: &mut KeyModel,
    n_keys: usize,
) -> Result<Vec<String>, Error> {
    let mut keys: Vec<String> = Vec::with_capacity(n_keys.min(PREALLOC_KEY_LIMIT));
    let mut key_buf = String::new();
    for _ in 0..n_keys {
        key_model.code_key(coder, "", &mut key_buf);
        keys.push(key_buf.clone());
        ensure_not_overrun(coder)?;
    }
    Ok(keys)
}

fn decode_counts(
    coder: &mut Coder,
    key_model: &mut KeyModel,
    keys: &[String],
) -> Result<Vec<usize>, Error> {
    let mut counts = Vec::with_capacity(keys.len());
    for key in keys {
        counts.push(key_model.code_count(coder, key, 0));
        ensure_not_overrun(coder)?;
    }
    Ok(counts)
}

struct DecodeRows<'a> {
    plan: &'a Plan,
    keys: &'a [String],
    counts: &'a [usize],
    text_len: u64,
}

fn decode_rows_into(coder: &mut Coder, lex: &mut Lexicon, rows: &DecodeRows) -> Result<(), Error> {
    let mut model = Model::new(rows.plan.clone());
    for (key, &group_len) in rows.keys.iter().zip(rows.counts) {
        model.start_group();
        for _ in 0..group_len {
            model.code_row(coder, &[], key);
            lex.push_row(model.row());
            if coder.overrun() || lex.data_len() as u64 > rows.text_len {
                return corrupt(OVERRUN_MESSAGE);
            }
        }
    }
    Ok(())
}

fn decode_irregular(coder: &mut Coder, lex: &mut Lexicon, n_irregular: usize) -> Result<(), Error> {
    let mut irregular_model = CharModel::new(IRREGULAR_CHAR_ORDER);
    let mut gaps = UintModel::default();
    let rows = lex.rows() as u64;
    let mut at = 0u64;
    for _ in 0..n_irregular {
        at = at.saturating_add(gaps.code(coder, 0));
        if at > rows {
            return corrupt("an irregular line lies past the last row");
        }
        let mut line = Vec::new();
        irregular_model.code_bytes(coder, &[], &mut line);
        lex.irregular_at.push(at as u32);
        lex.irregular.push(line);
        ensure_not_overrun(coder)?;
    }
    Ok(())
}

fn decode_coded(
    data: &[u8],
    h: &Header,
    n_keys: usize,
    n_irregular: usize,
    plan: &Plan,
) -> Result<Lexicon, Error> {
    let mut coder = Coder::decoder(&data[h.payload..]);
    let n_cols = plan.n_cols;
    let mut lex = Lexicon::new(n_cols);
    lex.no_final_newline = h.no_final_newline;
    if n_keys > MAX_KEY_COUNT || (n_cols == 0 && n_keys > 0) {
        return corrupt("bad key count");
    }
    let mut key_model = KeyModel::new();
    let keys = decode_keys(&mut coder, &mut key_model, n_keys)?;
    let counts = decode_counts(&mut coder, &mut key_model, &keys)?;
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
        let rows = DecodeRows {
            plan,
            keys: &keys,
            counts: &counts,
            text_len: h.text_len,
        };
        decode_rows_into(&mut coder, &mut lex, &rows)?;
    }
    decode_irregular(&mut coder, &mut lex, n_irregular)?;
    Ok(lex)
}

fn decode_unchecked(data: &[u8]) -> Result<(Lexicon, Header), Error> {
    let h = read_header(data)?;
    let lex = match &h.body {
        Body::Stored => decode_stored(data, &h)?,
        Body::Coded {
            n_keys,
            n_irregular,
            plan,
        } => decode_coded(data, &h, *n_keys, *n_irregular, plan)?,
    };
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
