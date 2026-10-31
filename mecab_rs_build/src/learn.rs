use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

use crate::build::text::{LINE_BUFFER_BYTES, Lines, key_value};
use mecab_rs::__private::files::io_error;
use mecab_rs::Error;

mod dict_gen;
mod eval;
mod features;
mod lattice;
mod lbfgs;
pub mod options;
mod tokenizer;
mod trainer;

pub use dict_gen::{GenerateOptions, generate};
pub use eval::{evaluate, test_sentences};
pub use trainer::{TrainOptions, TrainReport, train};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Existing {
    Keep,
    Replace,
}

#[derive(Default)]
pub(crate) struct Config(HashMap<String, String>);

impl Config {
    pub(crate) fn set(&mut self, key: &str, value: &str, existing: Existing) {
        if existing == Existing::Replace || !self.0.contains_key(key) {
            self.0.insert(key.to_owned(), value.to_owned());
        }
    }

    pub(crate) fn load(&mut self, path: &Path) -> Result<(), Error> {
        let bytes = std::fs::read(path).map_err(|_| {
            Error::learn(
                path,
                format!("no such file or directory: {}", path.display()),
            )
        })?;
        for line in bytes.split(|&byte| byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() || line[0] == b';' || line[0] == b'#' {
                continue;
            }
            let Some((key, value)) = key_value(line) else {
                return Err(Error::learn(
                    path,
                    format!("format error: {}", String::from_utf8_lossy(line)),
                ));
            };
            self.set(
                &String::from_utf8_lossy(key),
                &String::from_utf8_lossy(value),
                Existing::Keep,
            );
        }
        Ok(())
    }

    pub(crate) fn text(&self, key: &str) -> &str {
        self.0.get(key).map_or("", String::as_str)
    }

    pub(crate) fn number<T: std::str::FromStr + Default>(&self, key: &str) -> T {
        self.0
            .get(key)
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_default()
    }
}

const SIGNIFICANT_DIGITS: i32 = 6;
const LOWEST_FIXED_EXPONENT: i32 = -4;
const MANTISSA_DECIMALS: usize = 5;
const SCIENTIFIC_HAS_EXPONENT: &str = "scientific notation always has an exponent";
const SCIENTIFIC_EXPONENT_IS_INTEGER: &str = "a scientific exponent is an integer";
const FIXED_DECIMALS_NOT_NEGATIVE: &str = "a fixed exponent is below the significant digits";

fn split_scientific(value: f64) -> (String, i32) {
    let scientific = format!("{value:.MANTISSA_DECIMALS$e}");
    let (mantissa, exponent) = scientific.split_once('e').expect(SCIENTIFIC_HAS_EXPONENT);
    let exponent = exponent.parse().expect(SCIENTIFIC_EXPONENT_IS_INTEGER);
    (mantissa.to_owned(), exponent)
}

pub(crate) fn format_g(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "nan"
        } else if value < 0.0 {
            "-inf"
        } else {
            "inf"
        }
        .to_owned();
    }
    let (mantissa, exponent) = split_scientific(value);
    if !(LOWEST_FIXED_EXPONENT..SIGNIFICANT_DIGITS).contains(&exponent) {
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            &mantissa
        };
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let decimals =
        usize::try_from(SIGNIFICANT_DIGITS - 1 - exponent).expect(FIXED_DECIMALS_NOT_NEGATIVE);
    let fixed = format!("{value:.decimals$}");
    if fixed.contains('.') {
        fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        fixed
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineEnding {
    Lf,
    CrLf,
}

impl From<bool> for LineEnding {
    fn from(crlf: bool) -> Self {
        if crlf { Self::CrLf } else { Self::Lf }
    }
}

pub(crate) struct LineEnds<W: Write> {
    inner: W,
    ending: LineEnding,
}

impl<W: Write> LineEnds<W> {
    pub(crate) fn new(inner: W, ending: LineEnding) -> Self {
        Self { inner, ending }
    }
}

impl<W: Write> Write for LineEnds<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.ending == LineEnding::Lf {
            return self.inner.write(buffer);
        }
        let mut start = 0;
        for (at, &byte) in buffer.iter().enumerate() {
            if byte == b'\n' {
                self.inner.write_all(&buffer[start..at])?;
                self.inner.write_all(b"\r\n")?;
                start = at + 1;
            }
        }
        self.inner.write_all(&buffer[start..])?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn surface_and_feature(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let tab = line.iter().position(|&byte| byte == b'\t')?;
    let rest = &line[tab + 1..];
    let end = rest
        .iter()
        .position(|&byte| byte == b'\t')
        .unwrap_or(rest.len());
    Some((&line[..tab], &rest[..end]))
}

pub(crate) fn open_lines(path: &Path) -> Result<Lines, Error> {
    Lines::open(path, Some(LINE_BUFFER_BYTES)).map_err(|error| match error {
        Error::Io { .. } => Error::learn(
            path,
            format!("no such file or directory: {}", path.display()),
        ),
        other => other,
    })
}
