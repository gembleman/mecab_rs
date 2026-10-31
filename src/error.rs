use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

type Cause = Box<dyn std::error::Error + Send + Sync + 'static>;

const CHAIN_SEPARATOR: &str = ": ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IoAction {
    Open,
    Read,
    Seek,
    Inspect,
    Create,
    Copy,
    Write,
    Sync,
    Rename,
    Map,
}

impl IoAction {
    fn phrase(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::Seek => "seek in",
            Self::Inspect => "inspect",
            Self::Create => "create",
            Self::Copy => "copy",
            Self::Write => "write",
            Self::Sync => "sync",
            Self::Rename => "rename",
            Self::Map => "map",
        }
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io {
        action: IoAction,
        path: PathBuf,
        source: io::Error,
    },
    Format {
        file: PathBuf,
        line: Option<usize>,
        reason: Cow<'static, str>,
        source: Option<Cause>,
    },
    Output {
        source: io::Error,
    },
    MissingCharTable {
        path: PathBuf,
    },
    Tool {
        stage: ToolStage,
        path: PathBuf,
        line: Option<usize>,
        reason: String,
        source: Option<Cause>,
    },
    Unsupported(&'static str),
    NulInText,
    TextTooLong,
    UnsatisfiableConstraint,
}

impl Error {
    pub(crate) fn io(action: IoAction, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn output(source: io::Error) -> Self {
        Self::Output { source }
    }

    pub(crate) fn format(file: &Path, reason: impl Into<Cow<'static, str>>) -> Self {
        Self::Format {
            file: file.to_path_buf(),
            line: None,
            reason: reason.into(),
            source: None,
        }
    }

    pub(crate) fn format_at(
        file: &Path,
        line: usize,
        reason: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::format(file, reason).on_line(line)
    }

    pub(crate) fn tool(stage: ToolStage, path: &Path, reason: impl Into<String>) -> Self {
        Self::Tool {
            stage,
            path: path.to_path_buf(),
            line: None,
            reason: reason.into(),
            source: None,
        }
    }

    pub(crate) fn rebuild(path: &Path, reason: impl Into<String>) -> Self {
        Self::tool(ToolStage::Rebuild, path, reason)
    }

    pub(crate) fn caused_by(mut self, cause: impl Into<Cause>) -> Self {
        if let Self::Format { source, .. } | Self::Tool { source, .. } = &mut self {
            *source = Some(cause.into());
        }
        self
    }

    pub(crate) fn on_line(mut self, number: usize) -> Self {
        if let Self::Format { line, .. } | Self::Tool { line, .. } = &mut self {
            *line = Some(number);
        }
        self
    }

    pub fn report(&self) -> String {
        let mut text = self.to_string();
        let mut cause = std::error::Error::source(self);
        while let Some(error) = cause {
            text.push_str(CHAIN_SEPARATOR);
            text.push_str(&error.to_string());
            cause = error.source();
        }
        text
    }
}

pub(crate) fn io_error(action: IoAction, path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::io(action, path, source)
}

#[derive(Clone, Copy, Debug)]
pub struct Undecodable(pub &'static str);

impl Undecodable {
    pub(crate) fn in_file(self, file: &Path) -> Error {
        Error::format(file, self.0)
    }
}

impl fmt::Display for Undecodable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for Undecodable {}

fn write_location(
    formatter: &mut fmt::Formatter<'_>,
    path: &Path,
    line: Option<usize>,
) -> fmt::Result {
    write!(formatter, "{}", path.display())?;
    match line {
        Some(number) => write!(formatter, ":{number}"),
        None => Ok(()),
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { action, path, .. } => {
                write!(formatter, "cannot {} {}", action.phrase(), path.display())
            }
            Self::Format {
                file, line, reason, ..
            } => {
                formatter.write_str("invalid dictionary file ")?;
                write_location(formatter, file, *line)?;
                write!(formatter, ": {reason}")
            }
            Self::Output { .. } => formatter.write_str("cannot write the output"),
            Self::MissingCharTable { path } => write!(
                formatter,
                "{} does not exist: this crate reads char.uni instead of char.bin; create it with `mecab_rs::migrate_char_table` or the `mecab-rs-migrate` binary",
                path.display()
            ),
            Self::Tool {
                stage,
                path,
                line,
                reason,
                ..
            } => {
                write!(formatter, "cannot {} from ", stage.verb())?;
                write_location(formatter, path, *line)?;
                write!(formatter, ": {reason}")
            }
            Self::Unsupported(what) => write!(formatter, "unsupported: {what}"),
            Self::NulInText => formatter.write_str("the input text contains a NUL character"),
            Self::TextTooLong => {
                formatter.write_str("the input text is longer than u32::MAX bytes")
            }
            Self::UnsatisfiableConstraint => {
                formatter.write_str("no node satisfies the given constraints")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Output { source } => Some(source),
            Self::Format { source, .. } | Self::Tool { source, .. } => {
                source.as_deref().map(|cause| cause as _)
            }
            _ => None,
        }
    }
}
