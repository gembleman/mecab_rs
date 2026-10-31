use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use mecab_rs::__private::{
    CHAR_BIN_FILE, CHAR_DEF_FILE, CHAR_TABLE_FILE, HotShape, MATRIX_COST_BYTES,
    MATRIX_HEADER_BYTES, i16_at,
};
use mecab_rs::CharTable;
use mecab_rs_build::rebuild;
use mecab_rs_build::rebuild::{PAIR_COUNT_BYTES, PairCount, hot_shape_from_pair_counts};

const USAGE: &str = "usage: mecab-rs-migrate <dictionary-directory> [--char-def <file>]

Converts <dictionary-directory>/char.bin into <dictionary-directory>/char.uni. Range lines
above the BMP of char.def (the one in the directory, or the given file) are applied on top of
the built-in defaults; the other files of the directory are not touched.";

const REBUILD_USAGE: &str = "
dictionary rebuilding:
  mecab-rs-migrate pack <dict-dir> <out.mrd>
  mecab-rs-migrate pack-transport <dict-dir> <out.mrd.xz>
  mecab-rs-migrate expand <in.mrd.xz> <out.mrd>
      Writes a packed runtime dictionary, or packs and expands its transport form.
  mecab-rs-migrate check-runtime-int <matrix.itp> <runtime> [<matrix.bin>] [--threads N]
      Compares every cell of the runtime form of matrix.itp (what pack puts into the .mrd file)
      outside of its hot block with matrix.itp and, if given, with the dense matrix.bin.
  mecab-rs-migrate pair-stats <mrd> <corpus>... -o <file>  (feature pair-stats)
      Parses every line of the corpora with max grouping size 10 and writes how often the lattice
      asked for each (right node left id, left node right id) pair of the hot matrix as records
      of row u16, column u16, count u32 (little endian), for rebuild --pairs.
  mecab-rs-migrate rebuild <unidic-full-dir> <out-dir> (--stats <dir> | --corpus <file>...)
                         [--keep <fields>] [--threads N]
                         [--hot L R | --pairs <file> [--hot-cells N]]
                         [--matrix-bin]
      One-shot build of the runtime files only (dicrc, char.bin, char.uni, license/, sys.dic,
      unk.dic, left-id.def, right-id.def, right.tsv, left.tsv, matrix.itp, matrix.hot): renumbers
      by the stats (given, or counted from the corpora), slims with --keep, and compiles the
      integer templates matrix.itp (grid 2^-20, every cell exact) instead of writing matrix.bin;
      --matrix-bin also writes the permuted dense
      matrix.bin. --hot L R makes the hot part a rectangle; --pairs with --hot-cells N makes it a
      staircase of at most N cells (default 14,000,000) whose row lengths follow the pair counts.";

type Args = [std::ffi::OsString];

#[allow(clippy::needless_pass_by_value, reason = "used as a map_err callback")]
fn shown(error: mecab_rs::Error) -> String {
    error.to_string()
}

#[derive(Default)]
struct MatrixArgs {
    positional: Vec<PathBuf>,
    stats: Option<PathBuf>,
    corpora: Vec<PathBuf>,
    keep: Option<rebuild::Keep>,
    hot: Option<(usize, usize)>,
    hot_cells: Option<usize>,
    threads: Option<usize>,
    matrix_bin: bool,
    pairs: Option<PathBuf>,
}

fn parse_matrix_args(command: &str, args: &[std::ffi::OsString]) -> Result<MatrixArgs, String> {
    let mut parsed = MatrixArgs::default();
    let mut iterator = args.iter().peekable();
    let number = |value: Option<&std::ffi::OsString>, flag: &str| -> Result<usize, String> {
        value
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("{command}: {flag} needs a number"))
    };
    let text = |value: Option<&std::ffi::OsString>, flag: &str| -> Result<PathBuf, String> {
        value
            .map(PathBuf::from)
            .ok_or_else(|| format!("{command}: {flag} needs a value"))
    };
    while let Some(argument) = iterator.next() {
        match argument.to_str() {
            Some("--stats") => parsed.stats = Some(text(iterator.next(), "--stats")?),
            Some("--corpus") => {
                while let Some(file) = iterator
                    .next_if(|next| !next.to_str().is_some_and(|text| text.starts_with("--")))
                {
                    parsed.corpora.push(file.into());
                }
                if parsed.corpora.is_empty() {
                    return Err(format!("{command}: --corpus needs a file"));
                }
            }
            Some("--keep") => {
                let spec = text(iterator.next(), "--keep")?;
                parsed.keep = Some(rebuild::Keep::parse_fields(&spec.to_string_lossy())?);
            }
            Some("--hot") => {
                let left = number(iterator.next(), "--hot")?;
                parsed.hot = Some((left, number(iterator.next(), "--hot")?));
            }
            Some("--hot-cells") => {
                parsed.hot_cells = Some(number(iterator.next(), "--hot-cells")?);
            }
            Some("--threads") => parsed.threads = Some(number(iterator.next(), "--threads")?),
            Some("--pairs") => parsed.pairs = Some(text(iterator.next(), "--pairs")?),
            Some("--matrix-bin") => parsed.matrix_bin = true,
            Some(flag) if flag.starts_with("--") => {
                return Err(format!("{command}: unknown option {flag}"));
            }
            _ => parsed.positional.push(argument.into()),
        }
    }
    Ok(parsed)
}

const ROWS_PER_CHUNK: usize = 64;

type FirstMismatch = (usize, usize, i16, i16);

#[derive(Default)]
struct Comparison {
    compared: AtomicUsize,
    mismatches: AtomicUsize,
    first: Mutex<Option<FirstMismatch>>,
}

impl Comparison {
    fn compared_cell(&self) {
        self.compared.fetch_add(1, Ordering::Relaxed);
    }

    fn mismatch(&self, column: usize, row: usize, expected: i16, found: i16) {
        self.mismatches.fetch_add(1, Ordering::Relaxed);
        self.first
            .lock()
            .unwrap()
            .get_or_insert((column, row, expected, found));
    }

    fn mismatch_count(&self) -> usize {
        self.mismatches.load(Ordering::Relaxed)
    }

    fn first_mismatch(&self) -> Option<FirstMismatch> {
        *self.first.lock().unwrap()
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

fn positional<'a, const COUNT: usize>(
    command: &str,
    parsed: &'a MatrixArgs,
    usage: &str,
) -> Result<[&'a PathBuf; COUNT], String> {
    let found: Vec<&PathBuf> = parsed.positional.iter().collect();
    found
        .try_into()
        .map_err(|_| format!("{command}: expected {usage}"))
}

fn read_pair_counts(path: &Path) -> Result<Vec<PairCount>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let (chunks, rest) = bytes.as_chunks::<{ PAIR_COUNT_BYTES }>();
    if !rest.is_empty() {
        return Err(format!("{}: not a list of pair counts", path.display()));
    }
    Ok(chunks
        .iter()
        .map(|&chunk| PairCount::from_bytes(chunk))
        .collect())
}

fn hot_shape_of(parsed: &MatrixArgs) -> Result<Option<HotShape>, String> {
    match (&parsed.pairs, parsed.hot_cells, parsed.hot) {
        (Some(pairs), cells, None) => {
            let cells = cells.unwrap_or(rebuild::DEFAULT_HOT_CELLS);
            let shape = hot_shape_from_pair_counts(&read_pair_counts(pairs)?, cells);
            println!(
                "hot shape from pair counts: {} cells in {} rows, inscribed rectangle {:?}",
                shape.cells(),
                shape.rows(),
                shape.inscribed_rectangle()
            );
            Ok(Some(shape))
        }
        (None, None, hot) => Ok(hot.map(|(left, right)| HotShape::rectangle(left, right))),
        _ => Err("rebuild: give --pairs <file> with --hot-cells <N>, or --hot L R".into()),
    }
}

fn threads_of(parsed: &MatrixArgs) -> usize {
    parsed.threads.unwrap_or_else(rebuild::default_threads)
}

struct CheckArgs {
    positional: Vec<PathBuf>,
    threads: Option<usize>,
}

fn parse_check_args(args: &Args) -> Result<CheckArgs, String> {
    let mut parsed = CheckArgs {
        positional: Vec::new(),
        threads: None,
    };
    let mut iterator = args.iter();
    while let Some(argument) = iterator.next() {
        match argument.to_str() {
            Some("--threads") => {
                parsed.threads = Some(
                    iterator
                        .next()
                        .and_then(|value| value.to_str())
                        .and_then(|value| value.parse().ok())
                        .ok_or("check-runtime-int: --threads needs a number")?,
                );
            }
            Some(flag) if flag.starts_with("--") => {
                return Err(format!("check-runtime-int: unknown option {flag}"));
            }
            _ => parsed.positional.push(argument.into()),
        }
    }
    Ok(parsed)
}

fn read_templates(path: &Path) -> Result<mecab_rs::__private::IntTemplates, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    mecab_rs::__private::IntTemplates::from_file_bytes(&bytes, path).map_err(shown)
}

fn check_runtime_int(args: &Args) -> Result<(), String> {
    let parsed = parse_check_args(args)?;
    let usage = "<matrix.itp> <runtime.itp> [<matrix.bin>]";
    let (full, runtime, dense) = match parsed.positional.as_slice() {
        [full, runtime] => (full, runtime, None),
        [full, runtime, dense] => (full, runtime, Some(dense)),
        _ => return Err(format!("check-runtime-int: expected {usage}")),
    };
    let dense = dense.map(|path| read_dense(path)).transpose()?;
    let templates = read_templates(full)?;
    let full = mecab_rs::__private::MappedIntTemplates::open(full).map_err(shown)?;
    let runtime = mecab_rs::__private::RuntimeIntTemplates::open(runtime).map_err(shown)?;
    let (left_size, right_size) = (full.left_size(), full.right_size());
    if (left_size, right_size) != (runtime.left_size(), runtime.right_size()) {
        return Err("the two files have different sizes".into());
    }
    let (hot_left, hot_right) = (runtime.hot_left(), runtime.hot_right());
    if let Some(dense) = &dense
        && !dense_has_size(dense, left_size, right_size)
    {
        return Err("the dense matrix does not have the size of the templates".into());
    }
    let comparison = Comparison::default();
    compare_row_chunks(
        right_size,
        parsed.threads.unwrap_or_else(rebuild::default_threads),
        |rows| {
            for row in rows {
                for column in 0..left_size {
                    if row < hot_right && column < hot_left {
                        continue;
                    }
                    let (column_id, row_id) = (column as u16, row as u16);
                    let expected = full.cost(column_id, row_id);
                    let found = runtime.cost(column_id, row_id);
                    let by_templates = (row * 31 + column) % 61 == 0;
                    let real = dense
                        .as_ref()
                        .is_none_or(|dense| dense_cell(dense, left_size, column, row) == found);
                    comparison.compared_cell();
                    if !real
                        || found != expected
                        || (by_templates && templates.cost(column_id, row_id) != found)
                    {
                        comparison.mismatch(column, row, expected, found);
                    }
                }
            }
        },
    );
    let compared = comparison.compared.load(Ordering::Relaxed);
    let mismatches = comparison.mismatch_count();
    println!(
        "compared {compared} cells outside of the {hot_left} x {hot_right} hot block, {mismatches} mismatches"
    );
    match comparison.first_mismatch() {
        Some((column, row, expected, found)) => Err(format!(
            "the runtime form differs, first at rc {column} lc {row}: expected {expected}, found {found}"
        )),
        None => Ok(()),
    }
}

#[cfg(feature = "pair-stats")]
fn pair_stats(args: &Args) -> Result<(), String> {
    const STATS_MAX_GROUPING: usize = 10;
    let mut output = None;
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut iterator = args.iter();
    while let Some(argument) = iterator.next() {
        if argument == "-o" {
            output = Some(PathBuf::from(
                iterator.next().ok_or("pair-stats: -o needs a file")?,
            ));
        } else {
            inputs.push(argument.into());
        }
    }
    let output = output.ok_or("pair-stats: give the output file with -o")?;
    let Some((runtime, corpora)) = inputs.split_first().filter(|(_, rest)| !rest.is_empty()) else {
        return Err("pair-stats: expected <mrd> <corpus>... -o <file>".into());
    };
    let mut dictionary = mecab_rs::Dictionary::open(runtime).map_err(shown)?;
    dictionary.set_max_grouping_size(STATS_MAX_GROUPING);
    let mut tagger = mecab_rs::Tagger::new(&dictionary);
    let mut sentences = 0usize;
    for corpus in corpora {
        let bytes =
            std::fs::read(corpus).map_err(|error| format!("{}: {error}", corpus.display()))?;
        let text =
            std::str::from_utf8(&bytes).map_err(|_| format!("{}: not UTF-8", corpus.display()))?;
        for line in text.split_terminator('\n') {
            let sentence = line.strip_suffix('\r').unwrap_or(line);
            tagger.parse(sentence).map_err(shown)?;
            sentences += 1;
        }
    }
    let pairs: Vec<PairCount> = mecab_rs::__private::take_pair_counts()
        .into_iter()
        .map(|(row, column, count)| PairCount { row, column, count })
        .collect();
    let mut bytes = Vec::with_capacity(pairs.len() * PAIR_COUNT_BYTES);
    for pair in &pairs {
        bytes.extend_from_slice(&pair.to_bytes());
    }
    std::fs::write(&output, &bytes).map_err(|error| format!("{}: {error}", output.display()))?;
    println!(
        "{sentences} sentences, {} distinct pairs, {} lookups -> {}",
        pairs.len(),
        pairs.iter().map(|pair| u64::from(pair.count)).sum::<u64>(),
        output.display()
    );
    Ok(())
}

fn rebuild_command(args: &Args) -> Result<(), String> {
    let parsed = parse_matrix_args("rebuild", args)?;
    let [source, out] = positional("rebuild", &parsed, "<unidic-full-dir> <out-dir>")?;
    let stats = match (&parsed.stats, parsed.corpora.is_empty()) {
        (Some(directory), true) => rebuild::StatsSource::Directory(directory),
        (None, false) => rebuild::StatsSource::Corpora(&parsed.corpora),
        _ => return Err("rebuild: give either --stats <dir> or --corpus <file>...".into()),
    };
    let options = rebuild::RuntimeOptions {
        source,
        out,
        stats,
        keep: parsed.keep.clone(),
        hot: hot_shape_of(&parsed)?,
        threads: threads_of(&parsed),
        matrix_bin: parsed.matrix_bin,
    };
    let mut previous = Instant::now();
    let report = rebuild::build_runtime(&options, &mut |step| {
        println!("{step}: {:.2} s", previous.elapsed().as_secs_f64());
        previous = Instant::now();
    })
    .map_err(shown)?;
    println!(
        "built {} x {} ids into {}",
        report.left_size,
        report.right_size,
        out.display()
    );
    if let Some(slim) = report.slim {
        println!(
            "sys.dic features {} -> {} bytes, file {} -> {}",
            slim.old_feature_size, slim.new_feature_size, slim.old_file_size, slim.new_file_size
        );
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let all: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(command) = all.first().and_then(|first| first.to_str()) {
        let start = Instant::now();
        let args = &all[1..];
        let result = match command {
            "pack" => packed_command(command, args),
            "pack-transport" | "expand" => packed_command(command, args),
            "rebuild" => rebuild_command(args),
            #[cfg(feature = "pair-stats")]
            "pair-stats" => pair_stats(args),
            #[cfg(not(feature = "pair-stats"))]
            "pair-stats" => Err(format!(
                "{command}: this build has no pair counting; build with --features pair-stats"
            )),
            "check-runtime-int" => check_runtime_int(args),
            _ => return migrate_char_table(all),
        };
        result?;
        println!("{command}: {:.2} s", start.elapsed().as_secs_f64());
        return Ok(());
    }
    migrate_char_table(all)
}

fn packed_command(command: &str, args: &Args) -> Result<(), String> {
    if args.len() != 2 {
        return Err(format!("{command}: expected two paths"));
    }
    let input = Path::new(&args[0]);
    let output = Path::new(&args[1]);
    match command {
        "pack" => mecab_rs::packed::write_runtime(input, output).map_err(shown),
        "pack-transport" => mecab_rs_build::transport::write_transport(input, output)
            .map(|_| ())
            .map_err(shown),
        "expand" => mecab_rs::packed::expand(input, output).map_err(shown),
        _ => unreachable!(),
    }
}

fn migrate_char_table(all: Vec<std::ffi::OsString>) -> Result<(), String> {
    let mut directory: Option<PathBuf> = None;
    let mut char_def: Option<PathBuf> = None;
    let mut arguments = all.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "--char-def" {
            let file = arguments.next().ok_or("--char-def needs a file")?;
            char_def = Some(file.into());
        } else if argument == "-h" || argument == "--help" {
            println!(
                "{USAGE}
{REBUILD_USAGE}"
            );
            return Ok(());
        } else if directory.is_none() {
            directory = Some(argument.into());
        } else {
            return Err(format!(
                "unexpected argument {}",
                argument.to_string_lossy()
            ));
        }
    }
    let directory = directory.ok_or("no dictionary directory given")?;
    let default_def = directory.join(CHAR_DEF_FILE);
    let char_def: Option<&Path> = match &char_def {
        Some(file) => Some(file),
        None => default_def.is_file().then_some(default_def.as_path()),
    };
    let output = directory.join(CHAR_TABLE_FILE);
    CharTable::convert(&directory.join(CHAR_BIN_FILE), char_def, &output)
        .map_err(|error| error.to_string())?;
    println!("wrote {}", output.display());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mecab-rs-migrate: {message}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
