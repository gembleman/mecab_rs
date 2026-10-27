
pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

pub fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn parse_line(line: &str, buf: &mut Vec<u8>, ends: &mut Vec<u32>) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    loop {
        let start = buf.len();
        if i < b.len() && b[i] == b'"' {
            i += 1;
            loop {
                let Some(p) = b[i..].iter().position(|&x| x == b'"') else {
                    return false;
                };
                buf.extend_from_slice(&b[i..i + p]);
                i += p + 1;
                if i < b.len() && b[i] == b'"' {
                    buf.push(b'"');
                    i += 1;
                } else {
                    break;
                }
            }
            if !buf[start..].iter().any(|&x| x == b',' || x == b'"') {
                return false;
            }
            if i < b.len() && b[i] != b',' {
                return false;
            }
        } else {
            let p = b[i..]
                .iter()
                .position(|&x| x == b',')
                .unwrap_or(b.len() - i);
            let field = &b[i..i + p];
            if field.contains(&b'"') {
                return false;
            }
            buf.extend_from_slice(field);
            i += p;
        }
        if buf.len() > u32::MAX as usize {
            return false;
        }
        ends.push(buf.len() as u32);
        if i >= b.len() {
            return true;
        }
        i += 1;
        if i == b.len() {
            ends.push(buf.len() as u32);
            return true;
        }
    }
}

#[derive(Default)]
pub struct Lexicon {
    pub n_cols: usize,
    data: Vec<u8>,
    ends: Vec<u32>,
    pub irregular: Vec<Vec<u8>>,
    pub no_final_newline: bool,
}

impl Lexicon {
    pub fn new(n_cols: usize) -> Self {
        Lexicon {
            n_cols,
            ..Default::default()
        }
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
            self.ends.push(self.data.len() as u32);
        }
    }

    pub fn write_text<F: FnMut(&[u8])>(&self, order: Option<&[u32]>, mut sink: F) {
        let rows = self.rows();
        let total = rows + self.irregular.len();
        let mut emitted = 0usize;
        let mut emit_row = |r: usize, sink: &mut F| {
            for c in 0..self.n_cols {
                if c > 0 {
                    sink(b",");
                }
                let f = self.field_bytes(r, c);
                if f.iter().any(|&x| x == b',' || x == b'"') {
                    sink(b"\"");
                    let mut start = 0;
                    for (i, &x) in f.iter().enumerate() {
                        if x == b'"' {
                            sink(&f[start..=i]);
                            sink(b"\"");
                            start = i + 1;
                        }
                    }
                    sink(&f[start..]);
                    sink(b"\"");
                } else {
                    sink(f);
                }
            }
            emitted += 1;
            if emitted < total || !self.no_final_newline {
                sink(
                    b"
",
                );
            }
        };
        match order {
            Some(o) => {
                for &r in o {
                    emit_row(r as usize, &mut sink);
                }
            }
            None => {
                for r in 0..rows {
                    emit_row(r, &mut sink);
                }
            }
        }
        for l in &self.irregular {
            emitted += 1;
            sink(l);
            if emitted < total || !self.no_final_newline {
                sink(
                    b"
",
                );
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

pub struct Parsed {
    pub lex: Lexicon,
    pub order: Vec<u32>,
    pub already_grouped: bool,
}

pub fn parse_text(text: &[u8]) -> Parsed {
    let no_final_newline = !text.is_empty() && *text.last().unwrap() != b'\n';
    let body = if text.last() == Some(&b'\n') {
        &text[..text.len() - 1]
    } else {
        text
    };
    let lines = || body.split(|&b| b == b'\n');
    let mut counts: std::collections::HashMap<usize, usize> = Default::default();
    let mut buf = Vec::new();
    let mut ends = Vec::new();
    for l in lines().filter(|_| !text.is_empty()) {
        if let Ok(s) = std::str::from_utf8(l) {
            buf.clear();
            ends.clear();
            if parse_line(s, &mut buf, &mut ends) {
                *counts.entry(ends.len()).or_insert(0) += 1;
            }
        }
    }
    let n_cols = counts
        .iter()
        .max_by_key(|&(&k, &v)| (v, k))
        .map_or(0, |(&k, _)| k);
    let mut lex = Lexicon::new(n_cols);
    lex.no_final_newline = no_final_newline;
    let mut seen_irregular = false;
    let mut regular_after_irregular = false;
    if !text.is_empty() {
        for l in lines() {
            let mut ok = false;
            if let Ok(s) = std::str::from_utf8(l) {
                buf.clear();
                ends.clear();
                if parse_line(s, &mut buf, &mut ends) && ends.len() == n_cols {
                    ok = true;
                    let mut start = 0usize;
                    for &e in &ends {
                        lex.data.extend_from_slice(&buf[start..e as usize]);
                        start = e as usize;
                        lex.ends.push(lex.data.len() as u32);
                    }
                }
            }
            if ok {
                regular_after_irregular |= seen_irregular;
            } else {
                seen_irregular = true;
                lex.irregular.push(l.to_vec());
            }
        }
    }
    let rows = lex.rows();
    let mut order: Vec<u32> = (0..rows as u32).collect();
    order.sort_by(|&a, &b| {
        lex.field_bytes(a as usize, 0)
            .cmp(lex.field_bytes(b as usize, 0))
    });
    let sorted = order.iter().enumerate().all(|(i, &r)| i as u32 == r);
    Parsed {
        lex,
        order,
        already_grouped: sorted && !regular_after_irregular,
    }
}
