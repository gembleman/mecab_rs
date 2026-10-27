use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor};
use std::path::{Path, PathBuf};

use crate::error::Error;

pub(crate) const BUF_SIZE: usize = 8192;
const READ_BUFFER_BYTES: usize = 1 << 20;
const VERTICAL_TAB: u8 = 0x0b;

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
        .map_or(0, |offset| offset + 1);
    let value_start = line[equals + 1..]
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map_or(line.len(), |offset| equals + 1 + offset);
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
        && (first.is_ascii_whitespace() || *first == VERTICAL_TAB)
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
    while end < text.len() && (text[end].is_ascii_whitespace() || text[end] == VERTICAL_TAB) {
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
        .map_or(to, |offset| from + offset)
}

pub(crate) fn tokenize_csv(buffer: &mut [u8], max_columns: usize) -> Vec<usize> {
    let end_of_string = strlen(buffer);
    let mut columns_left = max_columns;
    let mut columns = Vec::new();
    let mut offset = 0;
    while offset < end_of_string {
        while buffer[offset] == b' ' || buffer[offset] == b'\t' {
            offset += 1;
        }
        let start;
        let end;
        if buffer[offset] == b'"' {
            offset += 1;
            start = offset;
            let mut written = start;
            while offset < end_of_string {
                if buffer[offset] == b'"' {
                    offset += 1;
                    if buffer[offset] != b'"' {
                        break;
                    }
                }
                buffer[written] = buffer[offset];
                written += 1;
                offset += 1;
            }
            offset = find(buffer, offset, end_of_string, b',');
            end = written;
        } else {
            start = offset;
            offset = find(buffer, offset, end_of_string, b',');
            end = offset;
        }
        if columns_left > 1 {
            buffer[end] = 0;
        }
        columns_left -= 1;
        columns.push(start);
        if columns_left == 0 {
            break;
        }
        offset += 1;
    }
    columns
}

pub(crate) fn tokenize2(buffer: &mut [u8], delimiters: &[u8], max_columns: usize) -> Vec<usize> {
    let end_of_string = strlen(buffer);
    let mut columns = Vec::new();
    let mut offset = 0;
    while columns.len() < max_columns {
        let next = buffer[offset..end_of_string]
            .iter()
            .position(|byte| delimiters.contains(byte))
            .map_or(end_of_string, |found| offset + found);
        buffer[next] = 0;
        if buffer[offset] != 0 {
            columns.push(offset);
        }
        if next == end_of_string {
            break;
        }
        offset = next + 1;
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
    c_buffer_size: Option<usize>,
}

impl Lines {
    pub(crate) fn open(path: &Path, c_buffer_size: Option<usize>) -> Result<Self, Error> {
        let file = File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::new(
            Box::new(BufReader::with_capacity(READ_BUFFER_BYTES, file)),
            path,
            c_buffer_size,
        ))
    }

    pub(crate) fn builtin(text: &'static str, path: &Path, c_buffer_size: Option<usize>) -> Self {
        Self::new(Box::new(Cursor::new(text.as_bytes())), path, c_buffer_size)
    }

    pub(crate) fn stdin(path: &Path, c_buffer_size: Option<usize>) -> Self {
        Self::new(Box::new(BufReader::new(io::stdin())), path, c_buffer_size)
    }

    fn new(reader: Box<dyn BufRead>, path: &Path, c_buffer_size: Option<usize>) -> Self {
        Self {
            reader,
            path: path.to_path_buf(),
            buffer: Vec::new(),
            c_buffer_size,
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
        if let Some(size) = self.c_buffer_size
            && self.buffer.len() > size - 2
        {
            return Err(failure(
                &self.path,
                format!("a line is longer than {} bytes", size - 2),
            ));
        }
        self.buffer.push(0);
        Ok(Some(&mut self.buffer))
    }
}
