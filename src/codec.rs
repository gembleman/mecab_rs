mod coder;
mod header;
mod key_model;
mod model;
mod plan_choice;
mod ppm;
mod table;
mod text;

use crate::error::Undecodable;
use crate::wire::{FNV_OFFSET, fnv1a};
use coder::{Coder, UintModel};
use header::{
    Body, CodedHeader, Header, OVERRUN_MESSAGE, charge_symbol, corrupt, ensure_not_overrun,
    header_flags, lines_fit_text, read_header, stored, write_coded_header,
};
use key_model::KeyModel;
use model::{Model, Plan};
use std::ops::Range;
use table::{Parsed, parse_text};
use text::CharModel;

pub(crate) use header::MAX_LEXICON_TEXT_LEN;
pub(crate) use table::Lexicon;

const PREALLOC_KEY_LIMIT: usize = 1 << 20;
const RESERVE_BYTES_PER_INPUT_BYTE: u64 = 1 << 10;
const IRREGULAR_CHAR_ORDER: usize = 2;
const SYMBOLS_PER_TEXT_BYTE: u64 = 3;
const SYMBOL_SLACK: u64 = 1 << 16;

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

fn encode_keys(input: &EncodeInput, key_model: &mut KeyModel, coder: &mut Coder) {
    let mut key_buf = String::new();
    for group in input.groups {
        key_model.code_key(coder, input.group_key(group), &mut key_buf);
    }
}

fn encode_counts(input: &EncodeInput, key_model: &mut KeyModel, coder: &mut Coder) {
    for group in input.groups {
        key_model.code_count(coder, input.group_key(group), group.len());
    }
}

fn encode_rows(input: &EncodeInput, plan: Plan, coder: &mut Coder) {
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
}

fn encode_irregular(lex: &Lexicon, coder: &mut Coder) {
    let mut irregular_model = CharModel::new(IRREGULAR_CHAR_ORDER);
    let mut gaps = UintModel::default();
    let mut line_buf = Vec::new();
    let mut previous_at = 0;
    for (line, &at) in lex.irregular.iter().zip(&lex.irregular_at) {
        gaps.code(coder, u64::from(at - previous_at));
        previous_at = at;
        irregular_model.code_bytes(coder, line, &mut line_buf);
    }
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

pub fn encode_lexicon(csv: &[u8]) -> Vec<u8> {
    debug_assert!(u32::try_from(csv.len()).is_ok());
    let parsed = parse_text(csv);
    let lex = &parsed.lex;
    let groups = group_bounds(&parsed);
    let input = EncodeInput {
        parsed: &parsed,
        groups: &groups,
    };
    let plan = plan_choice::choose_plan(&parsed, &groups);
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

    let mut coder = Coder::encoder();
    let mut key_model = KeyModel::new();
    encode_keys(&input, &mut key_model, &mut coder);
    encode_counts(&input, &mut key_model, &mut coder);
    encode_rows(&input, plan, &mut coder);
    encode_irregular(lex, &mut coder);
    out.extend_from_slice(&coder.finish());
    verified_or_stored(out, &parsed, text_len)
}

fn decode_stored(data: &[u8], h: &Header) -> Result<Lexicon, Undecodable> {
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
) -> Result<Vec<String>, Undecodable> {
    let mut keys: Vec<String> = Vec::with_capacity(n_keys.min(PREALLOC_KEY_LIMIT));
    let mut key_buf = String::new();
    for _ in 0..n_keys {
        charge_symbol(coder)?;
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
) -> Result<Vec<usize>, Undecodable> {
    let mut counts = Vec::with_capacity(keys.len());
    for key in keys {
        charge_symbol(coder)?;
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

fn decode_rows_into(
    coder: &mut Coder,
    lex: &mut Lexicon,
    rows: &DecodeRows,
) -> Result<(), Undecodable> {
    let mut model = Model::new(rows.plan.clone());
    for (key, &group_len) in rows.keys.iter().zip(rows.counts) {
        model.start_group();
        for _ in 0..group_len {
            charge_symbol(coder)?;
            model.code_row(coder, &[], key);
            lex.push_row(model.row());
            if coder.overrun() || lex.data_len() as u64 > rows.text_len {
                return corrupt(OVERRUN_MESSAGE);
            }
        }
    }
    Ok(())
}

fn decode_irregular(
    coder: &mut Coder,
    lex: &mut Lexicon,
    n_irregular: usize,
) -> Result<(), Undecodable> {
    let mut irregular_model = CharModel::new(IRREGULAR_CHAR_ORDER);
    let mut gaps = UintModel::default();
    let rows = lex.rows() as u64;
    let mut at = 0u64;
    for _ in 0..n_irregular {
        charge_symbol(coder)?;
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

fn reserve_hint(data_len: usize, text_len: u64) -> u64 {
    (data_len as u64)
        .saturating_mul(RESERVE_BYTES_PER_INPUT_BYTE)
        .min(text_len)
}

fn reserve_lexicon(lex: &mut Lexicon, total_rows: u64, hint_bytes: u64) -> Result<(), Undecodable> {
    let hint_rows = hint_bytes / lex.n_cols.max(1) as u64;
    if lex.reserve(total_rows.min(hint_rows) as usize, hint_bytes as usize) {
        Ok(())
    } else {
        corrupt("text too large")
    }
}

fn decode_coded(
    data: &[u8],
    h: &Header,
    n_keys: usize,
    n_irregular: usize,
    plan: &Plan,
) -> Result<Lexicon, Undecodable> {
    let mut coder = Coder::decoder(&data[h.payload..]);
    coder.limit_symbols(
        h.text_len
            .saturating_mul(SYMBOLS_PER_TEXT_BYTE)
            .saturating_add(SYMBOL_SLACK),
    );
    let n_cols = plan.n_cols;
    let mut lex = Lexicon::new(n_cols);
    lex.no_final_newline = h.no_final_newline;
    if n_cols == 0 && n_keys > 0 {
        return corrupt("bad key count");
    }
    let mut key_model = KeyModel::new();
    let keys = decode_keys(&mut coder, &mut key_model, n_keys)?;
    let counts = decode_counts(&mut coder, &mut key_model, &keys)?;
    let total_rows: u64 = counts.iter().map(|&group_len| group_len as u64).sum();
    if total_rows < n_keys as u64
        || !lines_fit_text(h.text_len, n_cols as u64, total_rows, n_irregular as u64)
    {
        return corrupt("bad row count");
    }
    if n_cols > 0 {
        let hint_bytes = reserve_hint(data.len() - h.payload, h.text_len);
        reserve_lexicon(&mut lex, total_rows, hint_bytes)?;
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

fn decode_unchecked(data: &[u8]) -> Result<(Lexicon, Header), Undecodable> {
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

pub fn decode_lexicon(data: &[u8]) -> Result<Vec<u8>, Undecodable> {
    let (lex, h) = decode_unchecked(data)?;
    let text = lex.to_text();
    if fnv1a(FNV_OFFSET, &text) != h.digest || text.len() as u64 != h.text_len {
        return corrupt("digest mismatch");
    }
    Ok(text)
}
