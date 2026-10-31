use crate::wire::{FNV_OFFSET, fnv1a};
use std::collections::HashMap;

fn read_quoted_field(bytes: &[u8], mut i: usize, buf: &mut Vec<u8>) -> Option<usize> {
    let start = buf.len();
    i += 1;
    loop {
        let p = bytes[i..].iter().position(|&x| x == b'"')?;
        buf.extend_from_slice(&bytes[i..i + p]);
        i += p + 1;
        if i < bytes.len() && bytes[i] == b'"' {
            buf.push(b'"');
            i += 1;
        } else {
            break;
        }
    }
    if !buf[start..].iter().any(|&x| x == b',' || x == b'"') {
        return None;
    }
    if i < bytes.len() && bytes[i] != b',' {
        return None;
    }
    Some(i)
}

fn read_plain_field(bytes: &[u8], i: usize, buf: &mut Vec<u8>) -> Option<usize> {
    let p = bytes[i..]
        .iter()
        .position(|&x| x == b',')
        .unwrap_or(bytes.len() - i);
    let field = &bytes[i..i + p];
    if field.contains(&b'"') {
        return None;
    }
    buf.extend_from_slice(field);
    Some(i + p)
}

pub(crate) fn parse_line(line: &str, buf: &mut Vec<u8>, ends: &mut Vec<u32>) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    loop {
        let next = if i < bytes.len() && bytes[i] == b'"' {
            read_quoted_field(bytes, i, buf)
        } else {
            read_plain_field(bytes, i, buf)
        };
        let Some(next) = next else {
            return false;
        };
        i = next;
        if buf.len() > u32::MAX as usize {
            return false;
        }
        ends.push(buf.len() as u32);
        if i >= bytes.len() {
            return true;
        }
        i += 1;
        if i == bytes.len() {
            ends.push(buf.len() as u32);
            return true;
        }
    }
}

impl std::fmt::Debug for Lexicon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Lexicon")
            .field("n_cols", &self.n_cols)
            .field("rows", &self.ends.len())
            .field("bytes", &self.data.len())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
pub struct Lexicon {
    pub n_cols: usize,
    data: Vec<u8>,
    ends: Vec<u32>,
    pub irregular: Vec<Vec<u8>>,
    pub irregular_at: Vec<u32>,
    pub no_final_newline: bool,
}

impl Lexicon {
    pub fn new(n_cols: usize) -> Self {
        Lexicon {
            n_cols,
            ..Default::default()
        }
    }

    fn end_offset(&self) -> u32 {
        u32::try_from(self.data.len()).unwrap_or(u32::MAX)
    }

    pub fn rows(&self) -> usize {
        self.ends.len().checked_div(self.n_cols).unwrap_or(0)
    }

    pub fn field(&self, row: usize, col: usize) -> &str {
        let i = row * self.n_cols + col;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        std::str::from_utf8(&self.data[start..self.ends[i] as usize]).unwrap_or("")
    }

    pub fn field_bytes(&self, row: usize, col: usize) -> &[u8] {
        let i = row * self.n_cols + col;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.data[start..self.ends[i] as usize]
    }

    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    pub fn push_row<S: AsRef<str>>(&mut self, fields: &[S]) {
        debug_assert_eq!(fields.len(), self.n_cols);
        for f in fields {
            self.data.extend_from_slice(f.as_ref().as_bytes());
            self.ends.push(self.end_offset());
        }
    }

    pub fn push_irregular(&mut self, line: Vec<u8>) {
        self.irregular_at.push(self.rows() as u32);
        self.irregular.push(line);
    }

    fn write_row<F: FnMut(&[u8])>(&self, row: usize, sink: &mut F) {
        for col in 0..self.n_cols {
            if col > 0 {
                sink(b",");
            }
            let field = self.field_bytes(row, col);
            if field.iter().any(|&x| x == b',' || x == b'"') {
                sink(b"\"");
                let mut start = 0;
                for (i, &x) in field.iter().enumerate() {
                    if x == b'"' {
                        sink(&field[start..=i]);
                        sink(b"\"");
                        start = i + 1;
                    }
                }
                sink(&field[start..]);
                sink(b"\"");
            } else {
                sink(field);
            }
        }
    }

    pub fn write_text<F: FnMut(&[u8])>(&self, order: Option<&[u32]>, mut sink: F) {
        debug_assert_eq!(self.irregular.len(), self.irregular_at.len());
        let rows = self.rows();
        let total = rows + self.irregular.len();
        let mut emitted = 0usize;
        let mut end_line = |sink: &mut F| {
            emitted += 1;
            if emitted < total || !self.no_final_newline {
                sink(b"\n");
            }
        };
        let mut next_irregular = 0usize;
        for slot in 0..=rows {
            while next_irregular < self.irregular.len()
                && (slot == rows || self.irregular_at[next_irregular] as usize <= slot)
            {
                sink(&self.irregular[next_irregular]);
                end_line(&mut sink);
                next_irregular += 1;
            }
            if slot < rows {
                let r = order.map_or(slot, |o| o[slot] as usize);
                self.write_row(r, &mut sink);
                end_line(&mut sink);
            }
        }
    }

    pub fn to_text(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + self.data.len() / 4 + 64);
        self.write_text(None, |p| out.extend_from_slice(p));
        out
    }

    pub fn reserve(&mut self, rows: usize, bytes: usize) -> bool {
        self.ends
            .try_reserve(rows.saturating_mul(self.n_cols))
            .is_ok()
            && self.data.try_reserve(bytes).is_ok()
    }

    pub fn digest(&self, order: Option<&[u32]>) -> (u64, u64) {
        let mut h = FNV_OFFSET;
        let mut len = 0u64;
        self.write_text(order, |p| {
            h = fnv1a(h, p);
            len += p.len() as u64;
        });
        (h, len)
    }
}

pub(crate) struct Parsed {
    pub lex: Lexicon,
    pub order: Vec<u32>,
    pub already_grouped: bool,
}

fn parse_utf8_line(line: &[u8], buf: &mut Vec<u8>, ends: &mut Vec<u32>) -> bool {
    let Ok(s) = std::str::from_utf8(line) else {
        return false;
    };
    buf.clear();
    ends.clear();
    parse_line(s, buf, ends)
}

fn dominant_col_count<'a>(
    lines: impl Iterator<Item = &'a [u8]>,
    buf: &mut Vec<u8>,
    ends: &mut Vec<u32>,
) -> usize {
    let mut lines_by_col_count: HashMap<usize, usize> = HashMap::new();
    for line in lines {
        if parse_utf8_line(line, buf, ends) {
            *lines_by_col_count.entry(ends.len()).or_insert(0) += 1;
        }
    }
    lines_by_col_count
        .iter()
        .max_by_key(|&(&k, &v)| (v, k))
        .map_or(0, |(&k, _)| k)
}

impl Lexicon {
    fn append_parsed_row(&mut self, buf: &[u8], ends: &[u32]) {
        let mut start = 0usize;
        for &e in ends {
            self.data.extend_from_slice(&buf[start..e as usize]);
            start = e as usize;
            self.ends.push(self.end_offset());
        }
    }
}

fn fill_lexicon<'a>(
    lex: &mut Lexicon,
    lines: impl Iterator<Item = &'a [u8]>,
    buf: &mut Vec<u8>,
    ends: &mut Vec<u32>,
) {
    for line in lines {
        if parse_utf8_line(line, buf, ends) && ends.len() == lex.n_cols {
            lex.append_parsed_row(buf, ends);
        } else {
            lex.push_irregular(line.to_vec());
        }
    }
}

fn sorted_order(lex: &Lexicon) -> (Vec<u32>, bool) {
    let mut order: Vec<u32> = (0..lex.rows() as u32).collect();
    order.sort_by(|&a, &b| {
        lex.field_bytes(a as usize, 0)
            .cmp(lex.field_bytes(b as usize, 0))
    });
    let sorted = order.iter().enumerate().all(|(i, &r)| i as u32 == r);
    (order, sorted)
}

pub(crate) fn parse_text(text: &[u8]) -> Parsed {
    let no_final_newline = text.last().is_some_and(|&last| last != b'\n');
    let body = if text.last() == Some(&b'\n') {
        &text[..text.len() - 1]
    } else {
        text
    };
    let lines = || body.split(|&b| b == b'\n').filter(|_| !text.is_empty());
    let mut buf = Vec::new();
    let mut ends = Vec::new();
    let n_cols = dominant_col_count(lines(), &mut buf, &mut ends);
    let mut lex = Lexicon::new(n_cols);
    lex.no_final_newline = no_final_newline;
    fill_lexicon(&mut lex, lines(), &mut buf, &mut ends);
    let (order, already_grouped) = sorted_order(&lex);
    Parsed {
        lex,
        order,
        already_grouped,
    }
}
