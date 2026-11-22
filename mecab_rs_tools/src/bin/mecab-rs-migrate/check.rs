use std::ffi::OsString;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use mecab_rs::__private::in_range_bytes::i16_at;
use mecab_rs::__private::matrix::{
    IntTemplates, MATRIX_COST_BYTES, MATRIX_HEADER_BYTES, MappedIntTemplates, RuntimeIntTemplates,
};
use mecab_rs_tools::cli_options::{Arity, NamedSpec, Options, lossy_arguments};
use mecab_rs_tools::rebuild;

use crate::shown;

const COMMAND: &str = "check-runtime-int";
const POSITIONAL_USAGE: &str = "<matrix.itp> <runtime.itp> [<matrix.bin>]";
const ROWS_PER_CHUNK: usize = 64;
const TEMPLATE_SAMPLE_STRIDE: usize = 31;
const TEMPLATE_SAMPLE_MODULUS: usize = 61;
const CELL_INSIDE_MATRIX: &str = "cell inside the matrix";

const CHECK_SPECS: [NamedSpec; 1] = [NamedSpec {
    name: "threads",
    arity: Arity::Numbers(1),
}];

type FirstMismatch = (usize, usize, i16, i16);

#[derive(Default)]
struct Comparison {
    compared: AtomicUsize,
    mismatches: AtomicUsize,
    first: Mutex<Option<FirstMismatch>>,
}

impl Comparison {
    fn first_slot(&self) -> MutexGuard<'_, Option<FirstMismatch>> {
        self.first.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn compared_cell(&self) {
        self.compared.fetch_add(1, Ordering::Relaxed);
    }

    fn mismatch(&self, column: usize, row: usize, expected: i16, found: i16) {
        self.mismatches.fetch_add(1, Ordering::Relaxed);
        self.first_slot()
            .get_or_insert((column, row, expected, found));
    }

    fn compared_count(&self) -> usize {
        self.compared.load(Ordering::Relaxed)
    }

    fn mismatch_count(&self) -> usize {
        self.mismatches.load(Ordering::Relaxed)
    }

    fn first_mismatch(&self) -> Option<FirstMismatch> {
        *self.first_slot()
    }
}

fn compare_row_chunks(
    right_size: usize,
    threads: usize,
    compare_rows: impl Fn(Range<usize>) + Sync,
) {
    let next_chunk = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let first = next_chunk.fetch_add(1, Ordering::Relaxed) * ROWS_PER_CHUNK;
                    if first >= right_size {
                        break;
                    }
                    compare_rows(first..(first + ROWS_PER_CHUNK).min(right_size));
                }
            });
        }
    });
}

fn dense_cell(dense: &[u8], left_size: usize, column: usize, row: usize) -> i16 {
    let at = MATRIX_HEADER_BYTES + MATRIX_COST_BYTES * (row * left_size + column);
    i16_at(dense, at)
}

fn dense_has_size(dense: &[u8], left_size: usize, right_size: usize) -> bool {
    dense.len() == MATRIX_HEADER_BYTES + MATRIX_COST_BYTES * left_size * right_size
}

fn read_dense(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn read_templates(path: &Path) -> Result<IntTemplates, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    IntTemplates::from_file_bytes(&bytes, path).map_err(shown)
}

struct CheckArgs {
    positional: Vec<PathBuf>,
    threads: Option<usize>,
}

fn parse_check_args(args: &[OsString]) -> Result<CheckArgs, String> {
    let options = Options::parse_named(COMMAND, &lossy_arguments(args), &CHECK_SPECS)?;
    Ok(CheckArgs {
        positional: options.rest.iter().map(PathBuf::from).collect(),
        threads: options.optional_number("threads"),
    })
}

struct Subjects {
    full: MappedIntTemplates,
    runtime: RuntimeIntTemplates,
    templates: IntTemplates,
    dense: Option<Vec<u8>>,
}

impl Subjects {
    fn open(full: &Path, runtime: &Path, dense: Option<&PathBuf>) -> Result<Self, String> {
        let dense = dense.map(|path| read_dense(path)).transpose()?;
        let templates = read_templates(full)?;
        let full = MappedIntTemplates::open(full).map_err(shown)?;
        let runtime = RuntimeIntTemplates::open(runtime).map_err(shown)?;
        let subjects = Self {
            full,
            runtime,
            templates,
            dense,
        };
        subjects.require_matching_sizes()?;
        Ok(subjects)
    }

    fn left_size(&self) -> usize {
        self.full.left_size()
    }

    fn right_size(&self) -> usize {
        self.full.right_size()
    }

    fn require_matching_sizes(&self) -> Result<(), String> {
        let (left_size, right_size) = (self.left_size(), self.right_size());
        if (left_size, right_size) != (self.runtime.left_size(), self.runtime.right_size()) {
            return Err("the two files have different sizes".into());
        }
        match &self.dense {
            Some(dense) if !dense_has_size(dense, left_size, right_size) => {
                Err("the dense matrix does not have the size of the templates".into())
            }
            _ => Ok(()),
        }
    }

    fn is_hot(&self, column: usize, row: usize) -> bool {
        row < self.runtime.hot_right() && column < self.runtime.hot_left()
    }

    fn check_cell(&self, column: usize, row: usize) -> Option<(i16, i16)> {
        let (column_id, row_id) = (column as u16, row as u16);
        let expected = self
            .full
            .try_cost(column_id, row_id)
            .expect(CELL_INSIDE_MATRIX);
        let found = self
            .runtime
            .try_cost(column_id, row_id)
            .expect(CELL_INSIDE_MATRIX);
        let dense_agrees = self
            .dense
            .as_ref()
            .is_none_or(|dense| dense_cell(dense, self.left_size(), column, row) == found);
        let sampled =
            (row * TEMPLATE_SAMPLE_STRIDE + column).is_multiple_of(TEMPLATE_SAMPLE_MODULUS);
        let templates_agree = !sampled
            || self
                .templates
                .try_cost(column_id, row_id)
                .expect(CELL_INSIDE_MATRIX)
                == found;
        (!dense_agrees || found != expected || !templates_agree).then_some((expected, found))
    }

    fn compare_rows(&self, rows: Range<usize>, comparison: &Comparison) {
        for row in rows {
            for column in (0..self.left_size()).filter(|&column| !self.is_hot(column, row)) {
                comparison.compared_cell();
                if let Some((expected, found)) = self.check_cell(column, row) {
                    comparison.mismatch(column, row, expected, found);
                }
            }
        }
    }
}

pub fn check_runtime_int(args: &[OsString]) -> Result<(), String> {
    let parsed = parse_check_args(args)?;
    let (full, runtime, dense) = match parsed.positional.as_slice() {
        [full, runtime] => (full, runtime, None),
        [full, runtime, dense] => (full, runtime, Some(dense)),
        _ => return Err(format!("{COMMAND}: expected {POSITIONAL_USAGE}")),
    };
    let subjects = Subjects::open(full, runtime, dense)?;
    let comparison = Comparison::default();
    compare_row_chunks(
        subjects.right_size(),
        parsed.threads.unwrap_or_else(rebuild::default_threads),
        |rows| subjects.compare_rows(rows, &comparison),
    );
    let (hot_left, hot_right) = (subjects.runtime.hot_left(), subjects.runtime.hot_right());
    println!(
        "compared {} cells outside of the {hot_left} x {hot_right} hot block, {} mismatches",
        comparison.compared_count(),
        comparison.mismatch_count()
    );
    match comparison.first_mismatch() {
        Some((column, row, expected, found)) => Err(format!(
            "the runtime form differs, first at rc {column} lc {row}: expected {expected}, found {found}"
        )),
        None => Ok(()),
    }
}
