
use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor};
use std::path::{Path, PathBuf};

use crate::error::Error;

pub(crate) const BUF_SIZE: usize = 8192;

pub(crate) fn failure(path: &Path, reason: impl Into<String>) -> Error {
    Error::Build {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

pub(crate) fn key_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let equals = line.iter().position(|&byte| byte == b'=')?;
    let key_end = line[..equals]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |at| at + 1);
    let value_start = line[equals + 1..]
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map_or(line.len(), |at| equals + 1 + at);
    Some((&line[..key_end], &line[value_start..]))
}

pub(crate) fn cstr(buffer: &[u8], start: usize) -> &[u8] {
    let rest = &buffer[start..];
    let length = rest
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(rest.len());
    &rest[..length]
}

fn strlen(buffer: &[u8]) -> usize {
    cstr(buffer, 0).len()
}

pub(crate) fn atoi(text: &[u8]) -> i32 {
    let mut rest = text;
    while let [first, tail @ ..] = rest
        && (first.is_ascii_whitespace() || *first == 0x0b)
    {
        rest = tail;
    }
    let negative = match rest {
        [b'-', tail @ ..] => {
            rest = tail;
            true
        }
        [b'+', tail @ ..] => {
            rest = tail;
            false
        }
        _ => false,
    };
    let mut value: i64 = 0;
    for digit in rest.iter().take_while(|byte| byte.is_ascii_digit()) {
        value = (value * 10 + i64::from(digit - b'0')).min(i64::from(i32::MAX) + 1);
    }
    let value = if negative { -value } else { value };
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

pub(crate) fn atof(text: &[u8]) -> f64 {
    let mut end = 0;
    let digits = |from: usize| {
        text[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    while end < text.len() && (text[end].is_ascii_whitespace() || text[end] == 0x0b) {
        end += 1;
    }
    let start = end;
    if matches!(text.get(end), Some(b'+' | b'-')) {
        end += 1;
    }
    let integer = digits(end);
    end += integer;
    let mut fraction = 0;
    if text.get(end) == Some(&b'.') {
        fraction = digits(end + 1);
        if integer > 0 || fraction > 0 {
            end += 1 + fraction;
        }
    }
    if integer == 0 && fraction == 0 {
        return 0.0;
    }
    if matches!(text.get(end), Some(b'e' | b'E')) {
        let mut exponent = end + 1;
        if matches!(text.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let exponent_digits = digits(exponent);
        if exponent_digits > 0 {
            end = exponent + exponent_digits;
        }
    }
    std::str::from_utf8(&text[start..end])
        .ok()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0.0)
}

fn find(buffer: &[u8], from: usize, to: usize, byte: u8) -> usize {
    buffer[from..to]
        .iter()
        .position(|&candidate| candidate == byte)
        .map_or(to, |at| from + at)
}

pub(crate) fn tokenize_csv(buffer: &mut [u8], max: usize) -> Vec<usize> {
    let end_of_string = strlen(buffer);
    let mut max = max;
    let mut columns = Vec::new();
    let mut at = 0;
    while at < end_of_string {
        while buffer[at] == b' ' || buffer[at] == b'\t' {
            at += 1;
        }
        let start;
        let end;
        if buffer[at] == b'"' {
            at += 1;
            start = at;
            let mut written = start;
            while at < end_of_string {
                if buffer[at] == b'"' {
                    at += 1;
                    if buffer[at] != b'"' {
                        break;
                    }
                }
                buffer[written] = buffer[at];
                written += 1;
                at += 1;
            }
            at = find(buffer, at, end_of_string, b',');
            end = written;
        } else {
            start = at;
            at = find(buffer, at, end_of_string, b',');
            end = at;
        }
        if max > 1 {
            buffer[end] = 0;
        }
        max -= 1;
        columns.push(start);
        if max == 0 {
            break;
        }
        at += 1;
    }
    columns
}

pub(crate) fn tokenize2(buffer: &mut [u8], delimiters: &[u8], max: usize) -> Vec<usize> {
    let end_of_string = strlen(buffer);
    let mut columns = Vec::new();
    let mut at = 0;
    while columns.len() < max {
        let next = buffer[at..end_of_string]
            .iter()
            .position(|byte| delimiters.contains(byte))
            .map_or(end_of_string, |found| at + found);
        buffer[next] = 0;
        if buffer[at] != 0 {
            columns.push(at);
        }
        if next == end_of_string {
            break;
        }
        at = next + 1;
    }
    columns
}

pub(crate) fn escape_csv_element(element: &mut Vec<u8>) {
    if element.contains(&b',') || element.contains(&b'"') {
        let mut quoted = vec![b'"'];
        for &byte in element.iter() {
            if byte == b'"' {
                quoted.push(b'"');
            }
            quoted.push(byte);
        }
        quoted.push(b'"');
        *element = quoted;
    }
}

pub(crate) struct Lines {
    reader: Box<dyn BufRead>,
    path: PathBuf,
    buffer: Vec<u8>,
    limit: Option<usize>,
}

impl Lines {
    pub(crate) fn open(path: &Path, limit: Option<usize>) -> Result<Self, Error> {
        let file = File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::new(
            Box::new(BufReader::with_capacity(1 << 20, file)),
            path,
            limit,
        ))
    }

    pub(crate) fn builtin(text: &'static str, path: &Path, limit: Option<usize>) -> Self {
        Self::new(Box::new(Cursor::new(text.as_bytes())), path, limit)
    }

    pub(crate) fn stdin(path: &Path, limit: Option<usize>) -> Self {
        Self::new(Box::new(BufReader::new(io::stdin())), path, limit)
    }

    fn new(reader: Box<dyn BufRead>, path: &Path, limit: Option<usize>) -> Self {
        Self {
            reader,
            path: path.to_path_buf(),
            buffer: Vec::new(),
            limit,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn next_line(&mut self) -> Result<Option<&mut [u8]>, Error> {
        self.buffer.clear();
        let read = self
            .reader
            .read_until(b'\n', &mut self.buffer)
            .map_err(|source| Error::Io {
                path: self.path.clone(),
                source,
            })?;
        if read == 0 {
            return Ok(None);
        }
        if self.buffer.last() == Some(&b'\n') {
            self.buffer.pop();
            if self.buffer.last() == Some(&b'\r') {
                self.buffer.pop();
            }
        }
        if let Some(limit) = self.limit
            && self.buffer.len() >= limit - 1
        {
            return Err(failure(
                &self.path,
                format!("a line is longer than {} bytes", limit - 2),
            ));
        }
        self.buffer.push(0);
        Ok(Some(&mut self.buffer))
    }
}
