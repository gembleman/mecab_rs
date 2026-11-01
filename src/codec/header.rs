use super::coder::Coder;
use super::hints::{read_hints, write_hints};
use super::model::Plan;
use super::ppm::PpmHint;
use crate::error::Undecodable;
use crate::wire::{FNV_OFFSET, fnv1a, get_varint, put_varint};

pub(super) const MAGIC: &[u8; 4] = b"MLX2";
const FLAG_NO_FINAL_NEWLINE: u8 = 1;
const FLAG_GROUPED: u8 = 2;
const FLAG_STORED: u8 = 4;
const FLAGS_KNOWN: u8 = 7;
const FLAGS_OFFSET: usize = 4;
const HEADER_PREFIX_LEN: usize = FLAGS_OFFSET + 1;
const DIGEST_BYTES: usize = 8;
const STORED_HEADER_SLACK: usize = 32;
pub(super) const OVERRUN_MESSAGE: &str = "truncated data";
pub(crate) const MAX_LEXICON_TEXT_LEN: u64 = 1 << 30;

pub(super) enum Body {
    Stored,
    Coded {
        n_keys: usize,
        n_irregular: usize,
        plan: Plan,
        hints: Vec<PpmHint>,
    },
}

impl Body {
    pub(super) fn hints(&self) -> &[PpmHint] {
        match self {
            Body::Stored => &[],
            Body::Coded { hints, .. } => hints,
        }
    }
}

pub(super) struct Header {
    pub body: Body,
    pub no_final_newline: bool,
    pub text_len: u64,
    pub digest: u64,
    pub payload: usize,
}

pub(super) fn corrupt<T>(why: &'static str) -> Result<T, Undecodable> {
    Err(Undecodable(why))
}

#[inline]
pub(super) fn ensure_not_overrun(coder: &Coder) -> Result<(), Undecodable> {
    if coder.overrun() {
        return corrupt(OVERRUN_MESSAGE);
    }
    Ok(())
}

#[inline]
pub(super) fn charge_symbol(coder: &mut Coder) -> Result<(), Undecodable> {
    if coder.spend_symbol() {
        Ok(())
    } else {
        corrupt(OVERRUN_MESSAGE)
    }
}

pub(super) fn header_flags(no_final_newline: bool, grouped: bool) -> u8 {
    let mut flags = 0;
    if no_final_newline {
        flags |= FLAG_NO_FINAL_NEWLINE;
    }
    if grouped {
        flags |= FLAG_GROUPED;
    }
    flags
}

pub(super) fn stored(text: &[u8], no_final_newline: bool, grouped: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + STORED_HEADER_SLACK);
    out.extend_from_slice(MAGIC);
    out.push(header_flags(no_final_newline, grouped) | FLAG_STORED);
    put_varint(&mut out, text.len() as u64);
    out.extend_from_slice(&fnv1a(FNV_OFFSET, text).to_le_bytes());
    out.extend_from_slice(text);
    out
}

pub(super) struct CodedHeader<'a> {
    pub flags: u8,
    pub text_len: u64,
    pub digest: u64,
    pub n_keys: usize,
    pub n_irregular: usize,
    pub plan: &'a Plan,
    pub hints: &'a [PpmHint],
}

pub(super) fn write_coded_header(out: &mut Vec<u8>, header: &CodedHeader) {
    out.extend_from_slice(MAGIC);
    out.push(header.flags);
    put_varint(out, header.text_len);
    out.extend_from_slice(&header.digest.to_le_bytes());
    put_varint(out, header.n_keys as u64);
    put_varint(out, header.n_irregular as u64);
    header.plan.write(out);
    write_hints(out, header.hints);
}

struct Prefix {
    flags: u8,
    text_len: u64,
    digest: u64,
    pos: usize,
}

fn read_prefix(data: &[u8]) -> Result<Prefix, Undecodable> {
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
    if text_len > MAX_LEXICON_TEXT_LEN {
        return corrupt("text too large");
    }
    let Some(digest) = data
        .get(pos..pos + DIGEST_BYTES)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    else {
        return corrupt("truncated header");
    };
    pos += DIGEST_BYTES;
    Ok(Prefix {
        flags,
        text_len,
        digest,
        pos,
    })
}

pub(super) fn lines_fit_text(text_len: u64, n_cols: u64, rows: u64, irregular: u64) -> bool {
    let min_bytes = u128::from(rows) * u128::from(n_cols) + u128::from(irregular);
    min_bytes <= u128::from(text_len) + 1 && n_cols <= text_len.saturating_add(1)
}

fn read_coded_body(data: &[u8], prefix: &mut Prefix) -> Result<Body, Undecodable> {
    let pos = &mut prefix.pos;
    let (Some(n_keys), Some(n_irregular)) = (get_varint(data, pos), get_varint(data, pos)) else {
        return corrupt("truncated header");
    };
    let Some(plan) = Plan::read(data, pos) else {
        return corrupt("bad plan");
    };
    if !lines_fit_text(prefix.text_len, plan.n_cols as u64, n_keys, n_irregular) {
        return corrupt("bad line count");
    }
    let Some(hints) = read_hints(data, pos) else {
        return corrupt("bad size hints");
    };
    Ok(Body::Coded {
        n_keys: n_keys as usize,
        n_irregular: n_irregular as usize,
        plan,
        hints,
    })
}

pub(super) fn read_header(data: &[u8]) -> Result<Header, Undecodable> {
    let mut prefix = read_prefix(data)?;
    let body = if prefix.flags & FLAG_STORED != 0 {
        Body::Stored
    } else {
        read_coded_body(data, &mut prefix)?
    };
    Ok(Header {
        body,
        no_final_newline: prefix.flags & FLAG_NO_FINAL_NEWLINE != 0,
        text_len: prefix.text_len,
        digest: prefix.digest,
        payload: prefix.pos,
    })
}
