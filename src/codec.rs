mod coder;
mod header;
mod hints;
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
use hints::HintReader;
use key_model::KeyModel;
use model::{Model, Plan};
use ppm::PpmHint;
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use table::{Parsed, parse_text};
use text::{CHAR_MODEL_PPMS, CharModel};

pub(crate) use header::MAX_LEXICON_TEXT_LEN;
pub(crate) use table::{Lexicon, Record, Row};

const PREALLOC_KEY_LIMIT: usize = 1 << 20;
const RESERVE_BYTES_PER_INPUT_BYTE: u64 = 1 << 10;
const IRREGULAR_CHAR_ORDER: usize = 2;
const SYMBOLS_PER_TEXT_BYTE: u64 = 3;
const SYMBOL_SLACK: u64 = 1 << 16;
const HEADER_SLACK: usize = 1 << 10;
const ROWS_PER_CHUNK: usize = 4096;
const CHUNK_DATA_HINT: usize = 1 << 18;
const CHUNKS_IN_FLIGHT: usize = 8;

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

fn encode_rows(
    input: &EncodeInput,
    plan: &Plan,
    coder: &mut Coder,
    hints: &mut Vec<PpmHint>,
    sizes: &mut Vec<u32>,
) {
    let lex = &input.parsed.lex;
    if lex.n_cols == 0 {
        return;
    }
    let mut model = Model::new(plan.clone());
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
    model.hints(hints);
    model.sizes(sizes);
}

fn encode_irregular(lex: &Lexicon, coder: &mut Coder, hints: &mut Vec<PpmHint>) {
    let mut irregular_model = CharModel::new(IRREGULAR_CHAR_ORDER, &mut HintReader::untrained());
    let mut gaps = UintModel::default();
    let mut line_buf = Vec::new();
    let mut previous_at = 0;
    for (line, &at) in lex.irregular.iter().zip(&lex.irregular_at) {
        gaps.code(coder, u64::from(at - previous_at));
        previous_at = at;
        irregular_model.code_bytes(coder, line, &mut line_buf);
    }
    irregular_model.hints(hints);
}

fn verified_or_stored(out: Vec<u8>, parsed: &Parsed, text_len: u64) -> Vec<u8> {
    if decode_digest_checked(&out).is_ok() {
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

    let mut coder = Coder::encoder();
    let mut hints = Vec::new();
    let mut sizes = Vec::new();
    let mut key_model = KeyModel::new(&mut HintReader::untrained());
    encode_keys(&input, &mut key_model, &mut coder);
    encode_counts(&input, &mut key_model, &mut coder);
    key_model.hints(&mut hints);
    drop(key_model);
    encode_rows(&input, &plan, &mut coder, &mut hints, &mut sizes);
    encode_irregular(lex, &mut coder, &mut hints);
    let payload = coder.finish();

    let mut out = Vec::with_capacity(payload.len() + HEADER_SLACK);
    write_coded_header(
        &mut out,
        &CodedHeader {
            flags: header_flags(lex.no_final_newline, parsed.already_grouped),
            text_len,
            digest,
            n_keys: groups.len(),
            n_irregular: lex.irregular.len(),
            plan: &plan,
            hints: &hints,
            sizes: &sizes,
        },
    );
    out.extend_from_slice(&payload);
    verified_or_stored(out, &parsed, text_len)
}

fn decode_digest_checked(data: &[u8]) -> Result<Lexicon, Undecodable> {
    let (lex, h) = decode_unchecked(data)?;
    let (digest, len) = lex.digest(None);
    if digest != h.digest || len != h.text_len {
        return corrupt("digest mismatch");
    }
    Ok(lex)
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

struct DecodeRows<'a, C> {
    plan: &'a Plan,
    keys: &'a [String],
    counts: &'a [C],
    text_len: u64,
}

fn symbol_budget(text_len: u64) -> u64 {
    text_len
        .saturating_mul(SYMBOLS_PER_TEXT_BYTE)
        .saturating_add(SYMBOL_SLACK)
}

fn decode_rows_with<C: Copy + Into<usize>>(
    coder: &mut Coder,
    rows: &DecodeRows<'_, C>,
    hints: &mut HintReader,
    mut push: impl FnMut(&[String]) -> u64,
) -> Result<(), Undecodable> {
    let mut model = Model::with_hints(rows.plan.clone(), hints);
    for (key, &group_len) in rows.keys.iter().zip(rows.counts) {
        model.start_group();
        for _ in 0..group_len.into() {
            charge_symbol(coder)?;
            model.code_row(coder, &[], key);
            let data_len = push(model.row());
            if coder.overrun() || data_len > rows.text_len {
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
    hints: &mut HintReader,
) -> Result<(), Undecodable> {
    let mut irregular_model = CharModel::new(IRREGULAR_CHAR_ORDER, hints);
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

struct CodedStart<'a> {
    coder: Coder<'a>,
    hints: HintReader<'a>,
    keys: Vec<String>,
    counts: Vec<usize>,
    total_rows: u64,
}

fn start_coded<'a>(
    data: &'a [u8],
    h: &'a Header,
    n_keys: usize,
    n_irregular: usize,
    plan: &Plan,
) -> Result<CodedStart<'a>, Undecodable> {
    let mut coder = Coder::decoder(&data[h.payload..]);
    coder.limit_symbols(symbol_budget(h.text_len));
    let mut hints = HintReader::new(h.body.hints(), h.body.sizes(), symbol_budget(h.text_len));
    let n_cols = plan.n_cols;
    if n_cols == 0 && n_keys > 0 {
        return corrupt("bad key count");
    }
    let mut key_model = KeyModel::new(&mut hints);
    let keys = decode_keys(&mut coder, &mut key_model, n_keys)?;
    let counts = decode_counts(&mut coder, &mut key_model, &keys)?;
    drop(key_model);
    let total_rows: u64 = counts.iter().map(|&group_len| group_len as u64).sum();
    if total_rows < n_keys as u64
        || !lines_fit_text(h.text_len, n_cols as u64, total_rows, n_irregular as u64)
    {
        return corrupt("bad row count");
    }
    Ok(CodedStart {
        coder,
        hints,
        keys,
        counts,
        total_rows,
    })
}

fn decode_coded(
    data: &[u8],
    h: &Header,
    n_keys: usize,
    n_irregular: usize,
    plan: &Plan,
) -> Result<Lexicon, Undecodable> {
    let CodedStart {
        mut coder,
        mut hints,
        keys,
        counts,
        total_rows,
    } = start_coded(data, h, n_keys, n_irregular, plan)?;
    let n_cols = plan.n_cols;
    let mut lex = Lexicon::new(n_cols);
    lex.no_final_newline = h.no_final_newline;
    if n_cols > 0 {
        let hint_bytes = reserve_hint(data.len() - h.payload, h.text_len);
        reserve_lexicon(&mut lex, total_rows, hint_bytes)?;
        let rows = DecodeRows {
            plan,
            keys: &keys,
            counts: &counts,
            text_len: h.text_len,
        };
        decode_rows_with(&mut coder, &rows, &mut hints, |fields| {
            lex.push_row(fields);
            lex.data_len() as u64
        })?;
    }
    decode_irregular(&mut coder, &mut lex, n_irregular, &mut hints)?;
    hints.finish()?;
    Ok(lex)
}

fn decode_body(data: &[u8], h: &Header) -> Result<Lexicon, Undecodable> {
    match &h.body {
        Body::Stored => decode_stored(data, h),
        Body::Coded {
            n_keys,
            n_irregular,
            plan,
            ..
        } => decode_coded(data, h, *n_keys, *n_irregular, plan),
    }
}

fn decode_unchecked(data: &[u8]) -> Result<(Lexicon, Header), Undecodable> {
    let h = read_header(data)?;
    let lex = decode_body(data, &h)?;
    Ok((lex, h))
}

pub(crate) use crate::packed::model::LexiconKeys;

pub(crate) fn visit_lexicon_records(
    data: &[u8],
    on_keys: impl FnOnce(Arc<LexiconKeys>) + Send,
    visit: impl FnMut(Record<'_>) -> Result<(), Undecodable>,
) -> Result<(), Undecodable> {
    let h = read_header(data)?;
    match &h.body {
        Body::Coded {
            n_keys,
            n_irregular: 0,
            plan,
            ..
        } if plan.n_cols > 0 => visit_pipelined(data, &h, (*n_keys, plan), on_keys, visit),
        _ => visit_decoded(&decode_body(data, &h)?, &h, visit),
    }
}

fn visit_decoded(
    lex: &Lexicon,
    h: &Header,
    visit: impl FnMut(Record<'_>) -> Result<(), Undecodable>,
) -> Result<(), Undecodable> {
    let (visited, checked) = std::thread::scope(|scope| {
        let checks = scope.spawn(|| (lex.digest(None), lex.fields_are_utf8()));
        let visited = lex.for_each_record(visit);
        (visited, checks.join().expect("the digest thread panicked"))
    });
    visited?;
    let ((digest, text_len), fields_are_utf8) = checked;
    finish_checks(h, digest, text_len, fields_are_utf8)
}

fn finish_checks(
    h: &Header,
    digest: u64,
    text_len: u64,
    fields_are_utf8: bool,
) -> Result<(), Undecodable> {
    if digest != h.digest || text_len != h.text_len {
        return corrupt("digest mismatch");
    }
    if !fields_are_utf8 {
        return corrupt("the lexicon is not UTF-8");
    }
    Ok(())
}

type SharedChunk = Arc<Lexicon>;

fn new_chunk(n_cols: usize) -> Lexicon {
    let mut chunk = Lexicon::new(n_cols);
    chunk.reserve(ROWS_PER_CHUNK, CHUNK_DATA_HINT);
    chunk
}

fn publish_chunk(
    records: &SyncSender<SharedChunk>,
    checks: &SyncSender<SharedChunk>,
    chunk: Lexicon,
) {
    let shared = Arc::new(chunk);
    let _ = records.send(Arc::clone(&shared));
    let _ = checks.send(shared);
}

fn narrow_counts(counts: Vec<usize>) -> Result<Vec<u8>, Undecodable> {
    counts
        .into_iter()
        .map(u8::try_from)
        .collect::<Result<Vec<u8>, _>>()
        .or_else(|_| corrupt("a key has more than 255 tokens"))
}

fn decode_chunks(
    data: &[u8],
    h: &Header,
    start: (usize, &Plan),
    on_keys: impl FnOnce(Arc<LexiconKeys>),
    records: SyncSender<SharedChunk>,
    checks: SyncSender<SharedChunk>,
) -> Result<(), Undecodable> {
    let (n_keys, plan) = start;
    let CodedStart {
        mut coder,
        mut hints,
        keys,
        counts,
        ..
    } = start_coded(data, h, n_keys, 0, plan)?;
    let shared = Arc::new(LexiconKeys {
        keys,
        counts: narrow_counts(counts)?,
    });
    on_keys(Arc::clone(&shared));
    let n_cols = plan.n_cols;
    let rows = DecodeRows {
        plan,
        keys: &shared.keys,
        counts: &shared.counts,
        text_len: h.text_len,
    };
    let mut chunk = new_chunk(n_cols);
    let mut total_bytes = 0u64;
    decode_rows_with(&mut coder, &rows, &mut hints, |fields| {
        let before = chunk.data_len();
        chunk.push_row(fields);
        total_bytes += (chunk.data_len() - before) as u64;
        if chunk.rows() == ROWS_PER_CHUNK {
            let full = std::mem::replace(&mut chunk, new_chunk(n_cols));
            publish_chunk(&records, &checks, full);
        }
        total_bytes
    })?;
    if chunk.rows() > 0 {
        publish_chunk(&records, &checks, chunk);
    }
    hints.skip(CHAR_MODEL_PPMS);
    hints.finish()?;
    drop(records);
    drop(checks);
    Ok(())
}

struct DigestStream {
    state: u64,
    len: u64,
}

impl DigestStream {
    fn feed(&mut self, bytes: &[u8]) {
        self.state = fnv1a(self.state, bytes);
        self.len += bytes.len() as u64;
    }
}

fn check_chunks(chunks: Receiver<SharedChunk>, no_final_newline: bool) -> ((u64, u64), bool) {
    let mut stream = DigestStream {
        state: FNV_OFFSET,
        len: 0,
    };
    let mut started = false;
    let mut fields_are_utf8 = true;
    for chunk in chunks {
        fields_are_utf8 &= chunk.fields_are_utf8();
        for row in chunk.row_iter() {
            if started {
                stream.feed(b"\n");
            }
            row.write_csv(&mut |piece| stream.feed(piece));
            started = true;
        }
    }
    if started && !no_final_newline {
        stream.feed(b"\n");
    }
    ((stream.state, stream.len), fields_are_utf8)
}

fn consume_chunks(
    chunks: Receiver<SharedChunk>,
    mut visit: impl FnMut(Record<'_>) -> Result<(), Undecodable>,
) -> Result<(), Undecodable> {
    let mut result = Ok(());
    for chunk in chunks {
        if result.is_err() {
            continue;
        }
        result = chunk.row_iter().try_for_each(|row| visit(Record::Row(row)));
    }
    result
}

fn visit_pipelined(
    data: &[u8],
    h: &Header,
    start: (usize, &Plan),
    on_keys: impl FnOnce(Arc<LexiconKeys>) + Send,
    visit: impl FnMut(Record<'_>) -> Result<(), Undecodable>,
) -> Result<(), Undecodable> {
    let (record_tx, record_rx) = sync_channel(CHUNKS_IN_FLIGHT);
    let (check_tx, check_rx) = sync_channel(CHUNKS_IN_FLIGHT);
    let no_final_newline = h.no_final_newline;
    let (decoded, visited, checked) = std::thread::scope(|scope| {
        let decoder =
            scope.spawn(move || decode_chunks(data, h, start, on_keys, record_tx, check_tx));
        let checks = scope.spawn(move || check_chunks(check_rx, no_final_newline));
        let visited = consume_chunks(record_rx, visit);
        (
            decoder.join().expect("the decoder thread panicked"),
            visited,
            checks.join().expect("the digest thread panicked"),
        )
    });
    decoded?;
    visited?;
    let ((digest, text_len), fields_are_utf8) = checked;
    finish_checks(h, digest, text_len, fields_are_utf8)
}
