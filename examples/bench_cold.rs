use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use mecab_rs::rebuild;

const USAGE: &str =
    "usage: bench_cold <templates> <templates> [--hot L R] [--samples N] [--pairs <file>]

Times the cold lookups of two template files (matrix.tpl, matrix.itp or the runtime form) on the
same cells for three rounds and fails if the sums of the costs differ. The cells are the
(column, row) pairs of --pairs (u16, u16 little endian), or else random cells outside of the
hot block (default size from rebuild, default 2000000 samples).";

const PAIR_BYTES: usize = 4;
const DEFAULT_SAMPLES: usize = 2_000_000;
const ROUNDS: usize = 3;
const NANOSECONDS: f64 = 1e9;
const XORSHIFT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

#[derive(Default)]
struct Arguments {
    files: Vec<PathBuf>,
    hot: Option<(usize, usize)>,
    samples: Option<usize>,
    pairs: Option<PathBuf>,
}

fn shown(error: mecab_rs::Error) -> String {
    error.to_string()
}

fn parse_arguments(args: &[OsString]) -> Result<Arguments, String> {
    let mut parsed = Arguments::default();
    let mut iterator = args.iter();
    let number = |value: Option<&OsString>, flag: &str| -> Result<usize, String> {
        value
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("{flag} needs a number"))
    };
    while let Some(argument) = iterator.next() {
        match argument.to_str() {
            Some("--hot") => {
                let left = number(iterator.next(), "--hot")?;
                parsed.hot = Some((left, number(iterator.next(), "--hot")?));
            }
            Some("--samples") => parsed.samples = Some(number(iterator.next(), "--samples")?),
            Some("--pairs") => {
                let value = iterator.next().ok_or("--pairs needs a value")?;
                parsed.pairs = Some(value.into());
            }
            Some(flag) if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ => parsed.files.push(argument.into()),
        }
    }
    Ok(parsed)
}

enum ColdFile {
    Float(mecab_rs::MappedTemplates),
    Full(mecab_rs::MappedIntTemplates),
    Runtime(mecab_rs::RuntimeIntTemplates),
}

impl ColdFile {
    fn open(path: &Path) -> Result<Self, String> {
        if let Ok(runtime) = mecab_rs::RuntimeIntTemplates::open(path) {
            return Ok(Self::Runtime(runtime));
        }
        if let Ok(full) = mecab_rs::MappedIntTemplates::open(path) {
            return Ok(Self::Full(full));
        }
        mecab_rs::MappedTemplates::open(path)
            .map(Self::Float)
            .map_err(shown)
    }

    fn sizes(&self) -> (usize, usize) {
        match self {
            Self::Float(file) => (file.left_size(), file.right_size()),
            Self::Full(file) => (file.left_size(), file.right_size()),
            Self::Runtime(file) => (file.left_size(), file.right_size()),
        }
    }

    fn cost(&self, column: u16, row: u16) -> i16 {
        match self {
            Self::Float(file) => file.cost(column, row),
            Self::Full(file) => file.cost(column, row),
            Self::Runtime(file) => file.cost(column, row),
        }
    }
}

fn recorded_cells(path: &Path) -> Result<Vec<(u16, u16)>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(bytes
        .as_chunks::<PAIR_BYTES>()
        .0
        .iter()
        .map(|&[a, b, c, d]| (u16::from_le_bytes([a, b]), u16::from_le_bytes([c, d])))
        .collect())
}

fn random_cells(parsed: &Arguments, (left_size, right_size): (usize, usize)) -> Vec<(u16, u16)> {
    let samples = parsed.samples.unwrap_or(DEFAULT_SAMPLES);
    let (hot_left, hot_right) = parsed
        .hot
        .unwrap_or((rebuild::DEFAULT_HOT_LEFT, rebuild::DEFAULT_HOT_RIGHT));
    let mut state = XORSHIFT_SEED;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut cells = Vec::with_capacity(samples);
    while cells.len() < samples {
        let column = (next() % left_size as u64) as u16;
        let row = (next() % right_size as u64) as u16;
        if usize::from(column) >= hot_left || usize::from(row) >= hot_right {
            cells.push((column, row));
        }
    }
    cells
}

fn run(args: &[OsString]) -> Result<(), String> {
    let parsed = parse_arguments(args)?;
    let [first_path, second_path] = parsed.files.as_slice() else {
        return Err("expected <templates> <templates>".into());
    };
    let (first, second) = (ColdFile::open(first_path)?, ColdFile::open(second_path)?);
    let cells = match &parsed.pairs {
        Some(pairs) => recorded_cells(pairs)?,
        None => random_cells(&parsed, first.sizes()),
    };
    let samples = cells.len();
    let time = |file: &ColdFile| {
        let started = Instant::now();
        let sum: i64 = cells
            .iter()
            .map(|&(column, row)| i64::from(file.cost(column, row)))
            .sum();
        (
            started.elapsed().as_secs_f64() * NANOSECONDS / samples as f64,
            sum,
        )
    };
    for round in 0..ROUNDS {
        let (first_ns, first_sum) = time(&first);
        let (second_ns, second_sum) = time(&second);
        println!(
            "round {round}: first {first_ns:.0} ns/cell, second {second_ns:.0} ns/cell (checksums {first_sum} {second_sum})"
        );
        if first_sum != second_sum {
            return Err("the two template files disagree".into());
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("bench_cold: {message}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
