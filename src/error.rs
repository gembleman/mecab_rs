use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Format {
        file: PathBuf,
        reason: &'static str,
    },
    CharDef {
        file: PathBuf,
        line: usize,
        reason: String,
    },
    MissingCharTable {
        path: PathBuf,
    },
    Rebuild {
        path: PathBuf,
        reason: String,
    },
    Build {
        path: PathBuf,
        reason: String,
    },
    Learn {
        path: PathBuf,
        reason: String,
    },
    Unsupported(&'static str),
    NulInText,
    TextTooLong,
    UnsatisfiableConstraint,
    Corrupt(&'static str),
}

pub fn io_error(path: &std::path::Path) -> impl Fn(io::Error) -> Error + '_ {
    |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Format { file, reason } => {
                write!(
                    formatter,
                    "invalid dictionary file {}: {reason}",
                    file.display()
                )
            }
            Self::CharDef {
                file,
                line: 0,
                reason,
            } => {
                write!(formatter, "invalid {}: {reason}", file.display())
            }
            Self::CharDef { file, line, reason } => {
                write!(
                    formatter,
                    "invalid {} at line {line}: {reason}",
                    file.display()
                )
            }
            Self::MissingCharTable { path } => write!(
                formatter,
                "{} does not exist: this crate reads char.uni instead of char.bin; create it                  with `mecab_rs::migrate_char_table` or the `mecab-rs-migrate` binary",
                path.display()
            ),
            Self::Rebuild { path, reason } => {
                write!(
                    formatter,
                    "cannot rebuild from {}: {reason}",
                    path.display()
                )
            }
            Self::Build { path, reason } => {
                write!(formatter, "cannot build from {}: {reason}", path.display())
            }
            Self::Learn { path, reason } => {
                write!(formatter, "cannot learn from {}: {reason}", path.display())
            }
            Self::Unsupported(what) => write!(formatter, "unsupported: {what}"),
            Self::NulInText => formatter.write_str("the input text contains a NUL character"),
            Self::TextTooLong => {
                formatter.write_str("the input text is longer than u32::MAX bytes")
            }
            Self::UnsatisfiableConstraint => {
                formatter.write_str("no node satisfies the given constraints")
            }
            Self::Corrupt(what) => write!(formatter, "the dictionary is corrupt: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub(crate) fn rebuild_error(path: &std::path::Path, reason: impl Into<String>) -> Error {
    Error::Rebuild {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}
