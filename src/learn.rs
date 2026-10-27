use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

use crate::build::text::{BUF_SIZE, Lines, key_value};
use crate::error::Error;

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

pub(crate) fn failure(path: &Path, reason: impl Into<String>) -> Error {
    Error::Learn {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

#[derive(Default)]
pub(crate) struct Config(HashMap<String, String>);

impl Config {
    pub(crate) fn set(&mut self, key: &str, value: &str, overwrite: bool) {
        if overwrite || !self.0.contains_key(key) {
            self.0.insert(key.to_owned(), value.to_owned());
        }
    }

    pub(crate) fn load(&mut self, path: &Path) -> Result<(), Error> {
        let bytes = std::fs::read(path).map_err(|_| {
            failure(
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
                return Err(failure(
                    path,
                    format!("format error: {}", String::from_utf8_lossy(line)),
                ));
            };
            self.set(
                &String::from_utf8_lossy(key),
                &String::from_utf8_lossy(value),
                false,
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
    let scientific = format!(
        "{value:.*e}",
        usize::try_from(SIGNIFICANT_DIGITS - 1).expect("at least 0")
    );
    let (mantissa, exponent) = scientific.split_once('e').expect("an exponent");
    let exponent: i32 = exponent.parse().expect("a number");
    if !(LOWEST_FIXED_EXPONENT..SIGNIFICANT_DIGITS).contains(&exponent) {
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let decimals = usize::try_from(SIGNIFICANT_DIGITS - 1 - exponent).expect("at least 0");
    let fixed = format!("{value:.decimals$}");
    if fixed.contains('.') {
        fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        fixed
    }
}

pub(crate) struct LineEnds<W: Write> {
    inner: W,
    crlf: bool,
}

impl<W: Write> LineEnds<W> {
    pub(crate) fn new(inner: W, crlf: bool) -> Self {
        Self { inner, crlf }
    }
}

impl<W: Write> Write for LineEnds<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if !self.crlf {
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
    Lines::open(path, Some(BUF_SIZE)).map_err(|error| match error {
        Error::Io { .. } => failure(
            path,
            format!("no such file or directory: {}", path.display()),
        ),
        other => other,
    })
}

pub(crate) fn io_error(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
