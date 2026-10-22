use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io { path: PathBuf, source: io::Error },
    Format { file: PathBuf, reason: &'static str },
    Unsupported(&'static str),
    NulInText,
    NoPath { rank: usize },
    TooLongSentence,
    NodeEndOutOfRange,
    UnsatisfiableConstraint,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "cannot read {}: {source}", path.display()),
            Self::Format { file, reason } => {
                write!(formatter, "invalid dictionary file {}: {reason}", file.display())
            }
            Self::Unsupported(what) => write!(formatter, "unsupported: {what}"),
            Self::NulInText => formatter.write_str("the input text contains a NUL character"),
            Self::NoPath { rank } => write!(formatter, "there is no path of rank {rank}"),
            Self::TooLongSentence => formatter.write_str("cannot connect the lattice: too long sentence"),
            Self::NodeEndOutOfRange => formatter.write_str("a node ends outside of the lattice"),
            Self::UnsatisfiableConstraint => {
                formatter.write_str("no node satisfies the given constraints")
            }
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
