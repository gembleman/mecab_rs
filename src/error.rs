use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolStage {
    Rebuild,
    Build,
    Learn,
}

impl ToolStage {
    fn verb(self) -> &'static str {
        match self {
            Self::Rebuild => "rebuild",
            Self::Build => "build",
            Self::Learn => "learn",
        }
    }
}

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
    Tool {
        stage: ToolStage,
        path: PathBuf,
        reason: String,
    },
    Unsupported(&'static str),
    NulInText,
    TextTooLong,
    UnsatisfiableConstraint,
    Corrupt(&'static str),
}

impl Error {
    pub fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    pub fn format(file: &Path, reason: &'static str) -> Self {
        Self::Format {
            file: file.to_path_buf(),
            reason,
        }
    }

    pub fn tool(stage: ToolStage, path: &Path, reason: impl Into<String>) -> Self {
        Self::Tool {
            stage,
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }

    pub fn rebuild(path: &Path, reason: impl Into<String>) -> Self {
        Self::tool(ToolStage::Rebuild, path, reason)
    }

    pub fn build(path: &Path, reason: impl Into<String>) -> Self {
        Self::tool(ToolStage::Build, path, reason)
    }

    pub fn learn(path: &Path, reason: impl Into<String>) -> Self {
        Self::tool(ToolStage::Learn, path, reason)
    }
}

pub fn io_error(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    |source| Error::io(path, source)
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
                "{} does not exist: this crate reads char.uni instead of char.bin; create it with `mecab_rs::migrate_char_table` or the `mecab-rs-migrate` binary",
                path.display()
            ),
            Self::Tool {
                stage,
                path,
                reason,
            } => {
                write!(
                    formatter,
                    "cannot {} from {}: {reason}",
                    stage.verb(),
                    path.display()
                )
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
