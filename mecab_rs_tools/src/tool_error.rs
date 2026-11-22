use std::path::Path;

use mecab_rs::__private::errors;
use mecab_rs::{Error, ToolStage};

pub(crate) use mecab_rs::__private::errors::{io_error, output as output_error};

pub(crate) fn build_error(path: &Path, reason: impl Into<String>) -> Error {
    errors::tool(ToolStage::Build, path, reason)
}

pub(crate) fn build_error_at(path: &Path, line: usize, reason: impl Into<String>) -> Error {
    errors::on_line(build_error(path, reason), line)
}

pub(crate) fn learn_error(path: &Path, reason: impl Into<String>) -> Error {
    errors::tool(ToolStage::Learn, path, reason)
}

pub(crate) fn learn_error_at(path: &Path, line: usize, reason: impl Into<String>) -> Error {
    errors::on_line(learn_error(path, reason), line)
}

pub(crate) fn rebuild_error(path: &Path, reason: impl Into<String>) -> Error {
    errors::tool(ToolStage::Rebuild, path, reason)
}

pub(crate) fn rebuild_error_at(path: &Path, line: usize, reason: impl Into<String>) -> Error {
    errors::on_line(rebuild_error(path, reason), line)
}

pub(crate) fn caused_by(
    error: Error,
    cause: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> Error {
    errors::caused_by(error, cause)
}

#[derive(Clone, Copy)]
pub(crate) struct Position<'a> {
    path: &'a Path,
    line: usize,
}

impl<'a> Position<'a> {
    pub(crate) fn new(path: &'a Path, line: usize) -> Self {
        Self { path, line }
    }

    pub(crate) fn build(self, reason: impl Into<String>) -> Error {
        build_error_at(self.path, self.line, reason)
    }

    pub(crate) fn learn(self, reason: impl Into<String>) -> Error {
        learn_error_at(self.path, self.line, reason)
    }
}
