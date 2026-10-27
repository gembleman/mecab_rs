use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use mecab_rs::{CharTable, rebuild};

const USAGE: &str = "usage: mecab-rs-migrate <dictionary-directory> [--char-def <file>]

Converts <dictionary-directory>/char.bin into <dictionary-directory>/char.uni. Range lines
above the BMP of char.def (the one in the directory, or the given file) are applied on top of
the built-in defaults; the other files of the directory are not touched.";

const REBUILD_USAGE: &str = "
dictionary rebuilding:
  mecab-rs-migrate pack <dict-dir> <out.mrd>
  mecab-rs-migrate pack-transport <dict-dir> <out.mrd.xz>  (feature xz)
  mecab-rs-migrate expand <in.mrd.xz> <out.mrd>           (feature xz)
      Writes a packed runtime dictionary, or packs and expands its transport form.
  mecab-rs-migrate stats <dict-dir> <out-dir> <corpus>...
      Analyzes every line of the corpora and writes right.tsv and left.tsv (id<TAB>count) to
      <out-dir>. Needs char.uni in <dict-dir>.
  mecab-rs-migrate renumber <dict-dir> <stats-dir> <out-dir> [--id-defs <dir>]
      Renumbers left/right ids by frequency: patches sys.dic and unk.dic, rewrites
      left-id.def/right-id.def and permutes matrix.bin; copies only dicrc, char.bin, char.uni
      and license/.
  mecab-rs-migrate slim <dict-dir> <out-dir> --keep <fields>
  mecab-rs-migrate slim <dict-dir> <out-dir> <count> [--pad]
      Rewrites sys.dic keeping only the listed feature fields (for example 0-7,9,10,12,20,22;
      the others become `*`, in features of 29 fields only), or the first <count> fields.
      Copies the runtime files (dicrc, char.bin, char.uni, license/, unk.dic, left-id.def,
      right-id.def, matrix.bin, matrix.hot, matrix.tpl, matrix.xz, right.tsv, left.tsv).
  mecab-rs-migrate templates <source-dir> <dict-dir> [--id-defs <dir>] [--hot L R] [--threads N]
      Compiles the connection templates of <source-dir> (feature.def, model.def, dicrc) for
      the id definitions in <id-defs-dir> (default: <source-dir>) and writes matrix.tpl and
      matrix.hot into <dict-dir>. The hot block defaults to 4480 x 3136 (clamped).
  mecab-rs-migrate check-templates <source-dir> <matrix.bin> [--id-defs <dir>] [--threads N]
      Compiles the templates like `templates` and compares the cost of every cell with the
      dense matrix.bin; exits with an error at the first mismatch.
  mecab-rs-migrate pack-matrix <source-dir> <out.xz> [--id-defs <dir>]      (feature xz)
  mecab-rs-migrate unpack-matrix <file.xz> <dict-dir> [--hot L R] [--threads N]   (feature xz)
      Packs the compiled templates into one xz file, or writes matrix.tpl and matrix.hot
      from such a file.
  mecab-rs-migrate pack-int <source-dir> <out.xz> [--id-defs <dir>] [--shift N | --shift A-B]
                            [--reference <matrix.bin>] [--threads N]           (feature xz)
  mecab-rs-migrate unpack-int <file.xz> <id-defs-dir> <dict-dir> [--hot L R] [--threads N]
                                                                                (feature xz)
  mecab-rs-migrate check-int <file.xz> <id-defs-dir> <matrix.bin> [--threads N]  (feature xz)
  mecab-rs-migrate bench-cold <templates> <templates> [--hot L R] [--samples N] [--pairs <file>]
      Integer matrix: rounds the template weights to the grid 2^-shift (the shifts given, default
      16-26), lists the cells where the integer sum differs from the real matrix (taken from
      --reference or else from the exact templates) and writes the smallest xz file; unpack-int
      writes matrix.itp and matrix.hot from it; check-int compares every cell of the decoded
      integer matrix with matrix.bin; bench-cold times the cold lookups of two template files
      (matrix.tpl, matrix.itp or the runtime form).
  mecab-rs-migrate runtime-int <matrix.itp> <matrix.hot> <out>
  mecab-rs-migrate check-runtime-int <matrix.itp> <runtime> [<matrix.bin>] [--threads N]
      Writes the runtime form of matrix.itp (what pack puts into the .mrd file) for the hot block
      of matrix.hot, and compares every cell outside of the hot block with matrix.itp and, if
      given, with the dense matrix.bin.
  mecab-rs-migrate pair-stats <mrd> <corpus>... -o <file>                  (feature pair-stats)
      Parses every line of the corpora with max grouping size 10 and writes how often the lattice
      asked for each (right node left id, left node right id) pair of the hot matrix as records
      of row u16, column u16, count u32 (little endian), for rebuild --pairs.
  mecab-rs-migrate pair-cover <matrix.hot> <pairs>
      Prints how many of the lookups counted in <pairs> fall inside the hot part of <matrix.hot>.
  mecab-rs-migrate rebuild <unidic-full-dir> <out-dir> (--stats <dir> | --corpus <file>...)
                         [--keep <fields>] [--threads N] [--int [--shift N]]
                         [--hot L R | --pairs <file> [--hot-cells N]]
                         [--xz] [--matrix-bin]
      One-shot build of the runtime files only (dicrc, char.bin, char.uni, license/, sys.dic,
      unk.dic, left-id.def, right-id.def, right.tsv, left.tsv, matrix.tpl, matrix.hot): renumbers
      by the stats (given, or counted from the corpora), slims with --keep, and compiles the
      templates instead of writing matrix.bin. --int writes the integer templates matrix.itp
      (grid 2^-20 unless --shift, every cell exact, about half of matrix.tpl) in place of
      matrix.tpl. --xz also writes matrix.xz (feature xz), --matrix-bin the permuted dense
      matrix.bin. --hot L R makes the hot part a rectangle; --pairs with --hot-cells N makes it a
      staircase of at most N cells (default 14,000,000) whose row lengths follow the pair counts.";

type Args = [std::ffi::OsString];

fn path_argument(command: &str, args: &Args, index: usize, what: &str) -> Result<PathBuf, String> {
    args.get(index)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{command}: missing {what}"))
}

fn stats(args: &Args) -> Result<(), String> {
    let (dictionary, out) = (
        path_argument("stats", args, 0, "<dict-dir>")?,
        path_argument("stats", args, 1, "<out-dir>")?,
    );
    let corpora: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();
    if corpora.is_empty() {
        return Err("stats: no corpus given".into());
    }
    let stats = rebuild::collect_stats(&dictionary, &corpora).map_err(shown)?;
    stats.write_tsv(&out).map_err(shown)?;
    let seen = |counts: &[u64]| counts.iter().filter(|&&count| count > 0).count();
    println!(
        "lookups {} rc seen {} lc seen {}",
        stats.right.iter().sum::<u64>(),
        seen(&stats.right),
        seen(&stats.left)
    );
    Ok(())
}

fn renumber(args: &Args) -> Result<(), String> {
    let (dictionary, stats, out) = (
        path_argument("renumber", args, 0, "<dict-dir>")?,
        path_argument("renumber", args, 1, "<stats-dir>")?,
        path_argument("renumber", args, 2, "<out-dir>")?,
    );
    let id_defs = match args.get(3) {
        None => None,
        Some(flag) if flag == "--id-defs" => Some(path_argument(
            "renumber",
            args,
            4,
            "the --id-defs directory",
        )?),
        Some(other) => {
            return Err(format!("unexpected argument {}", other.to_string_lossy()));
        }
    };
    if args.len() > 5 {
        return Err("renumber: too many arguments".into());
    }
    let report = rebuild::renumber(&dictionary, &stats, &out, id_defs.as_deref()).map_err(shown)?;
    println!(
        "renumbered {} x {} ids into {}",
        report.left_size,
        report.right_size,
        out.display()
    );
    Ok(())
}

fn slim(args: &Args) -> Result<(), String> {
    let (dictionary, out) = (
        path_argument("slim", args, 0, "<dict-dir>")?,
        path_argument("slim", args, 1, "<out-dir>")?,
    );
    let keep = match args.get(2).and_then(|argument| argument.to_str()) {
        Some("--keep") => {
            let spec = args.get(3).and_then(|spec| spec.to_str());
            rebuild::Keep::parse_fields(spec.ok_or("slim: --keep needs a field set")?)?
        }
        Some(count) => rebuild::Keep::Prefix {
            keep: count
                .parse()
                .map_err(|_| format!("slim: `{count}` is not a field count"))?,
            pad: args.get(3).is_some_and(|flag| flag == "--pad"),
        },
        None => return Err("slim: give --keep <fields> or a field count".into()),
    };
    let report = rebuild::slim(&dictionary, &out, &keep).map_err(shown)?;
    println!(
        "sys.dic features {} -> {} bytes, {} unique, file {} -> {}",
        report.old_feature_size,
        report.new_feature_size,
        report.unique_features,
        report.old_file_size,
        report.new_file_size
    );
    Ok(())
}

fn shown(error: mecab_rs::Error) -> String {
    error.to_string()
}

#[derive(Default)]
struct MatrixArgs {
    positional: Vec<PathBuf>,
    id_defs: Option<PathBuf>,
    stats: Option<PathBuf>,
    corpora: Vec<PathBuf>,
    keep: Option<rebuild::Keep>,
    hot: Option<(usize, usize)>,
    hot_cells: Option<usize>,
    threads: Option<usize>,
    xz: bool,
    int: bool,
    matrix_bin: bool,
    shifts: Option<Vec<u32>>,
    reference: Option<PathBuf>,
    samples: Option<usize>,
    pairs: Option<PathBuf>,
}

fn parse_shifts(text: &str) -> Result<Vec<u32>, String> {
    let bad = || format!("`{text}` is not a shift or a range like 16-26");
    let (first, last) = text.split_once('-').unwrap_or((text, text));
    let first: u32 = first.parse().map_err(|_| bad())?;
    let last: u32 = last.parse().map_err(|_| bad())?;
    if first > last || last > rebuild::int_matrix::MAX_SHIFT {
        return Err(bad());
    }
    Ok((first..=last).collect())
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
            Some("--id-defs") => parsed.id_defs = Some(text(iterator.next(), "--id-defs")?),
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
            Some("--shift") => {
                let value = text(iterator.next(), "--shift")?;
                parsed.shifts = Some(parse_shifts(&value.to_string_lossy())?);
            }
            Some("--reference") => parsed.reference = Some(text(iterator.next(), "--reference")?),
            Some("--pairs") => parsed.pairs = Some(text(iterator.next(), "--pairs")?),
            Some("--samples") => parsed.samples = Some(number(iterator.next(), "--samples")?),
            Some("--xz") => parsed.xz = true,
            Some("--int") => parsed.int = true,
            Some("--matrix-bin") => parsed.matrix_bin = true,
            Some(flag) if flag.starts_with("--") => {
                return Err(format!("{command}: unknown option {flag}"));
            }
            _ => parsed.positional.push(argument.into()),
        }
    }
    Ok(parsed)
}

fn read_id_defs(source: &Path, id_defs: Option<&Path>) -> Result<(String, String), String> {
    let read = |name: &str| {
        let path = id_defs.unwrap_or(source).join(name);
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
    };
    Ok((read("left-id.def")?, read("right-id.def")?))
}

fn compile(source: &Path, id_defs: Option<&Path>) -> Result<mecab_rs::ConnectionTemplates, String> {
    let (left, right) = read_id_defs(source, id_defs)?;
    rebuild::compile_templates(source, &left, &right).map_err(|error| error.to_string())
}

fn check_templates(
    templates: &mecab_rs::ConnectionTemplates,
    matrix: &Path,
    threads: usize,
) -> Result<(), String> {
    let bytes = std::fs::read(matrix).map_err(|error| format!("{}: {error}", matrix.display()))?;
    let (left_size, right_size) = (templates.left_size(), templates.right_size());
    if bytes.len() != 4 + 2 * left_size * right_size
        || usize::from(u16::from_le_bytes([bytes[0], bytes[1]])) != left_size
        || usize::from(u16::from_le_bytes([bytes[2], bytes[3]])) != right_size
    {
        return Err("the matrix does not have the size of the templates".into());
    }
    let next_row = std::sync::atomic::AtomicUsize::new(0);
    let mismatches = std::sync::atomic::AtomicUsize::new(0);
    let first = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let row = next_row.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if row >= right_size {
                        break;
                    }
                    let cells = &bytes[4 + 2 * row * left_size..][..2 * left_size];
                    for (rc, cell) in cells.as_chunks::<2>().0.iter().enumerate() {
                        let expected = i16::from_le_bytes(*cell);
                        let found = templates.cost(rc as u16, row as u16);
                        if expected != found {
                            mismatches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            first
                                .lock()
                                .unwrap()
                                .get_or_insert((rc, row, expected, found));
                        }
                    }
                }
            });
        }
    });
    let mismatches = mismatches.into_inner();
    println!(
        "compared {} cells, {mismatches} mismatches",
        left_size * right_size
    );
    match first.into_inner().unwrap() {
        Some((rc, lc, expected, found)) => Err(format!(
            "first mismatch at rc {rc} lc {lc}: matrix {expected}, templates {found}"
        )),
        None => Ok(()),
    }
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

fn rectangle_of(parsed: &MatrixArgs) -> Option<mecab_rs::HotShape> {
    parsed
        .hot
        .map(|(left, right)| mecab_rs::HotShape::rectangle(left, right))
}

fn read_pair_counts(path: &Path) -> Result<Vec<mecab_rs::PairCount>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let (chunks, rest) = bytes.as_chunks::<{ mecab_rs::PAIR_COUNT_BYTES }>();
    if !rest.is_empty() {
        return Err(format!("{}: not a list of pair counts", path.display()));
    }
    Ok(chunks
        .iter()
        .map(|&chunk| mecab_rs::PairCount::from_bytes(chunk))
        .collect())
}

fn hot_shape_of(parsed: &MatrixArgs) -> Result<Option<mecab_rs::HotShape>, String> {
    match (&parsed.pairs, parsed.hot_cells, parsed.hot) {
        (Some(pairs), cells, None) => {
            let cells = cells.unwrap_or(rebuild::DEFAULT_HOT_CELLS);
            let shape = mecab_rs::HotShape::from_pair_counts(&read_pair_counts(pairs)?, cells);
            println!(
                "hot shape from pair counts: {} cells in {} rows, inscribed rectangle {:?}",
                shape.cells(),
                shape.rows(),
                shape.inscribed_rectangle()
            );
            Ok(Some(shape))
        }
        (None, None, hot) => {
            Ok(hot.map(|(left, right)| mecab_rs::HotShape::rectangle(left, right)))
        }
        _ => Err("rebuild: give --pairs <file> with --hot-cells <N>, or --hot L R".into()),
    }
}

fn threads_of(parsed: &MatrixArgs) -> usize {
    parsed.threads.unwrap_or_else(rebuild::default_threads)
}

fn templates(args: &Args, start: Instant) -> Result<(), String> {
    let parsed = parse_matrix_args("templates", args)?;
    let [source, dictionary] = positional("templates", &parsed, "<source-dir> <dict-dir>")?;
    let templates = compile(source, parsed.id_defs.as_deref())?;
    println!("compiled in {:.2} s", start.elapsed().as_secs_f64());
    std::fs::create_dir_all(dictionary).map_err(|error| error.to_string())?;
    rebuild::write_matrix_files(
        &templates,
        dictionary,
        rectangle_of(&parsed),
        threads_of(&parsed),
    )
    .map_err(shown)
}

fn check_templates_command(args: &Args, start: Instant) -> Result<(), String> {
    let parsed = parse_matrix_args("check-templates", args)?;
    let [source, matrix] = positional("check-templates", &parsed, "<source-dir> <matrix.bin>")?;
    let templates = compile(source, parsed.id_defs.as_deref())?;
    println!("compiled in {:.2} s", start.elapsed().as_secs_f64());
    check_templates(&templates, matrix, threads_of(&parsed))
}

#[cfg(feature = "xz")]
fn pack_int(args: &Args, start: Instant) -> Result<(), String> {
    let parsed = parse_matrix_args("pack-int", args)?;
    let [source, out] = positional("pack-int", &parsed, "<source-dir> <out.xz>")?;
    let shifts = parsed
        .shifts
        .clone()
        .unwrap_or_else(|| rebuild::int_matrix::DEFAULT_SHIFTS.collect());
    let (left, right) = read_id_defs(source, parsed.id_defs.as_deref())?;
    let templates = compile(source, parsed.id_defs.as_deref())?;
    println!("compiled in {:.2} s", start.elapsed().as_secs_f64());
    let (reports, chosen) = rebuild::int_matrix::pack_int(
        &templates,
        source,
        &left,
        &right,
        &shifts,
        parsed.reference.as_deref(),
        threads_of(&parsed),
        out,
        &mut |line| println!("{line}"),
    )
    .map_err(shown)?;
    for report in &reports {
        let sizes = &report.sizes;
        println!(
            "shift {:2}: exceptions {:>9}  xz header {} pattern {} weights {} exceptions {} total {}  |  ideal pattern {:.0} weights {:.0} exceptions {:.0} total {:.0}",
            report.shift,
            report.exceptions,
            sizes.header.1,
            sizes.pattern.1,
            sizes.weights.1,
            sizes.exceptions.1,
            sizes.total.1,
            report.ideal.pattern,
            report.ideal.weights,
            report.ideal.exceptions,
            report.ideal.total(),
        );
    }
    println!(
        "chosen shift {}: {} bytes in {}",
        reports[chosen].shift,
        reports[chosen].sizes.total.1,
        out.display()
    );
    Ok(())
}

#[cfg(feature = "xz")]
fn unpack_int(args: &Args) -> Result<(), String> {
    let parsed = parse_matrix_args("unpack-int", args)?;
    let [pack, id_defs, dictionary] =
        positional("unpack-int", &parsed, "<file.xz> <id-defs-dir> <dict-dir>")?;
    std::fs::create_dir_all(dictionary).map_err(|error| error.to_string())?;
    rebuild::int_matrix::unpack_int(
        pack,
        id_defs,
        dictionary,
        rectangle_of(&parsed),
        threads_of(&parsed),
    )
    .map_err(shown)
}

#[cfg(feature = "xz")]
fn check_int(args: &Args, start: Instant) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let parsed = parse_matrix_args("check-int", args)?;
    let [pack, id_defs, matrix] =
        positional("check-int", &parsed, "<file.xz> <id-defs-dir> <matrix.bin>")?;
    let (left, right) = read_id_defs(id_defs, None)?;
    let templates = rebuild::int_matrix::read_int_pack(pack, &left, &right).map_err(shown)?;
    println!(
        "decoded in {:.2} s: shift {}, {} exceptions",
        start.elapsed().as_secs_f64(),
        templates.shift(),
        templates.exception_count()
    );
    let bytes = std::fs::read(matrix).map_err(|error| format!("{}: {error}", matrix.display()))?;
    let (left_size, right_size) = (templates.left_size(), templates.right_size());
    if bytes.len() != 4 + 2 * left_size * right_size {
        return Err("the matrix does not have the size of the templates".into());
    }
    const ROWS: usize = 64;
    let next_chunk = AtomicUsize::new(0);
    let mismatches = AtomicUsize::new(0);
    let first = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads_of(&parsed).max(1) {
            scope.spawn(|| {
                loop {
                    let chunk = next_chunk.fetch_add(1, Ordering::Relaxed);
                    if chunk * ROWS >= right_size {
                        break;
                    }
                    templates.visit_rows(chunk * ROWS, (chunk + 1) * ROWS, |row, costs| {
                        let cells = &bytes[4 + 2 * row * left_size..][..2 * left_size];
                        for (column, (cell, &found)) in
                            cells.as_chunks::<2>().0.iter().zip(costs).enumerate()
                        {
                            let expected = i16::from_le_bytes(*cell);
                            if expected != found {
                                mismatches.fetch_add(1, Ordering::Relaxed);
                                first
                                    .lock()
                                    .unwrap()
                                    .get_or_insert((column, row, expected, found));
                            }
                        }
                    });
                }
            });
        }
    });
    let cells = left_size * right_size;
    let exception_cells: Vec<usize> = templates
        .exceptions()
        .map(|(cell, _)| cell as usize)
        .collect();
    let mut single = 0usize;
    for cell in (0..cells).step_by(997).chain(exception_cells) {
        let (column, row) = (cell % left_size, cell / left_size);
        let expected = i16::from_le_bytes([bytes[4 + 2 * cell], bytes[5 + 2 * cell]]);
        if templates.cost(column as u16, row as u16) != expected {
            mismatches.fetch_add(1, Ordering::Relaxed);
        }
        single += 1;
    }
    let mismatches = mismatches.into_inner();
    println!("compared {cells} cells by rows and {single} by cell, {mismatches} mismatches");
    match first.into_inner().unwrap() {
        Some((column, row, expected, found)) => Err(format!(
            "first mismatch at rc {column} lc {row}: matrix {expected}, integer templates {found}"
        )),
        None if mismatches > 0 => Err("the single-cell path differs".into()),
        None => Ok(()),
    }
}

fn runtime_int(args: &Args) -> Result<(), String> {
    let parsed = parse_matrix_args("runtime-int", args)?;
    let [itp, hot, out] = positional("runtime-int", &parsed, "<matrix.itp> <matrix.hot> <out>")?;
    let ints = mecab_rs::MappedIntTemplates::open(itp).map_err(shown)?;
    let matrix = std::fs::read(hot).map_err(|error| format!("{}: {error}", hot.display()))?;
    let (_, _, shape) = mecab_rs::HotShape::parse(&matrix)
        .map_err(|reason| format!("{}: {reason}", hot.display()))?;
    let bytes = ints.runtime_form(&shape).map_err(shown)?;
    std::fs::write(out, &bytes).map_err(|error| format!("{}: {error}", out.display()))?;
    println!("wrote {} bytes", bytes.len());
    Ok(())
}

fn check_runtime_int(args: &Args) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let parsed = parse_matrix_args("check-runtime-int", args)?;
    let usage = "<matrix.itp> <runtime.itp> [<matrix.bin>]";
    let (full, runtime, dense) = match parsed.positional.as_slice() {
        [full, runtime] => (full, runtime, None),
        [full, runtime, dense] => (full, runtime, Some(dense)),
        _ => return Err(format!("check-runtime-int: expected {usage}")),
    };
    let dense = dense
        .map(|path| std::fs::read(path).map_err(|error| format!("{}: {error}", path.display())))
        .transpose()?;
    let full = mecab_rs::MappedIntTemplates::open(full).map_err(shown)?;
    let runtime = mecab_rs::RuntimeIntTemplates::open(runtime).map_err(shown)?;
    let (left_size, right_size) = (full.left_size(), full.right_size());
    if (left_size, right_size) != (runtime.left_size(), runtime.right_size()) {
        return Err("the two files have different sizes".into());
    }
    let (hot_left, hot_right) = (runtime.hot_left(), runtime.hot_right());
    if let Some(dense) = &dense
        && dense.len() != 4 + 2 * left_size * right_size
    {
        return Err("the dense matrix does not have the size of the templates".into());
    }
    let next_row = AtomicUsize::new(0);
    let checked = AtomicUsize::new(0);
    let mismatches = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads_of(&parsed).max(1) {
            scope.spawn(|| {
                loop {
                    let row = next_row.fetch_add(1, Ordering::Relaxed);
                    if row >= right_size {
                        break;
                    }
                    for column in 0..left_size {
                        if row < hot_right && column < hot_left {
                            continue;
                        }
                        let (column, row) = (column as u16, row as u16);
                        let expected = full.cost(column, row);
                        let found = runtime.cost(column, row);
                        let by_templates = (usize::from(row) * 31 + usize::from(column)) % 61 == 0;
                        let real = dense.as_ref().is_none_or(|dense| {
                            let at = 4 + 2 * (usize::from(row) * left_size + usize::from(column));
                            i16::from_le_bytes([dense[at], dense[at + 1]]) == found
                        });
                        if !real
                            || found != expected
                            || (by_templates && full.cost_from_templates(column, row) != found)
                        {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                        checked.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let (checked, mismatches) = (checked.into_inner(), mismatches.into_inner());
    println!(
        "compared {checked} cells outside of the {hot_left} x {hot_right} hot block, {mismatches} mismatches"
    );
    if mismatches > 0 {
        return Err("the runtime form differs".into());
    }
    Ok(())
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

const PAIR_BYTES: usize = 4;

fn recorded_cells(path: &Path) -> Result<Vec<(u16, u16)>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(bytes
        .as_chunks::<PAIR_BYTES>()
        .0
        .iter()
        .map(|&[a, b, c, d]| (u16::from_le_bytes([a, b]), u16::from_le_bytes([c, d])))
        .collect())
}

fn random_cells(parsed: &MatrixArgs, (left_size, right_size): (usize, usize)) -> Vec<(u16, u16)> {
    let samples = parsed.samples.unwrap_or(2_000_000);
    let (hot_left, hot_right) = parsed
        .hot
        .unwrap_or((rebuild::DEFAULT_HOT_LEFT, rebuild::DEFAULT_HOT_RIGHT));
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
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

fn bench_cold(args: &Args) -> Result<(), String> {
    let parsed = parse_matrix_args("bench-cold", args)?;
    let [first_path, second_path] = positional("bench-cold", &parsed, "<templates> <templates>")?;
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
        (started.elapsed().as_secs_f64() * 1e9 / samples as f64, sum)
    };
    for round in 0..3 {
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
    let pairs = mecab_rs::pair_stats::take();
    let mut bytes = Vec::with_capacity(pairs.len() * mecab_rs::PAIR_COUNT_BYTES);
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

fn pair_cover(args: &Args) -> Result<(), String> {
    let [hot, pairs] = match args {
        [hot, pairs] => [Path::new(hot), Path::new(pairs)],
        _ => return Err("pair-cover: expected <matrix.hot> <pairs>".into()),
    };
    let matrix = std::fs::read(hot).map_err(|error| format!("{}: {error}", hot.display()))?;
    let (_, _, shape) = mecab_rs::HotShape::parse(&matrix)
        .map_err(|reason| format!("{}: {reason}", hot.display()))?;
    let (mut inside, mut outside) = (0u64, 0u64);
    for pair in read_pair_counts(pairs)? {
        let count = u64::from(pair.count);
        if shape.contains(usize::from(pair.row), usize::from(pair.column)) {
            inside += count;
        } else {
            outside += count;
        }
    }
    println!(
        "{} cells in {} rows: {inside} lookups inside, {outside} outside ({:.2}%)",
        shape.cells(),
        shape.rows(),
        outside as f64 * 100.0 / (inside + outside).max(1) as f64
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
        int_shift: match parsed.shifts.as_deref() {
            _ if !parsed.int => None,
            None => Some(rebuild::int_matrix::RUNTIME_SHIFT),
            Some(&[shift]) => Some(shift),
            Some(_) => return Err("rebuild: --int takes one --shift".into()),
        },
        xz: parsed.xz,
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
    if let Some(size) = report.xz_size {
        println!("matrix.xz: {size} bytes");
    }
    Ok(())
}

#[cfg(feature = "xz")]
fn pack_matrix(args: &Args, start: Instant) -> Result<(), String> {
    let parsed = parse_matrix_args("pack-matrix", args)?;
    let [source, out] = positional("pack-matrix", &parsed, "<source-dir> <out.xz>")?;
    let templates = compile(source, parsed.id_defs.as_deref())?;
    println!("compiled in {:.2} s", start.elapsed().as_secs_f64());
    let size = rebuild::xz::pack(&templates, out).map_err(shown)?;
    println!("{}: {size} bytes", out.display());
    Ok(())
}

#[cfg(feature = "xz")]
fn unpack_matrix(args: &Args) -> Result<(), String> {
    let parsed = parse_matrix_args("unpack-matrix", args)?;
    let [pack, dictionary] = positional("unpack-matrix", &parsed, "<file.xz> <dict-dir>")?;
    std::fs::create_dir_all(dictionary).map_err(|error| error.to_string())?;
    rebuild::xz::unpack(pack, dictionary, rectangle_of(&parsed), threads_of(&parsed)).map_err(shown)
}

fn run() -> Result<(), String> {
    let all: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(command) = all.first().and_then(|first| first.to_str()) {
        let start = Instant::now();
        let args = &all[1..];
        let result = match command {
            "pack" => packed_command(command, args),
            #[cfg(feature = "xz")]
            "pack-transport" | "expand" => packed_command(command, args),
            "stats" => stats(args),
            "renumber" => renumber(args),
            "slim" => slim(args),
            "templates" => templates(args, start),
            "check-templates" => check_templates_command(args, start),
            "rebuild" => rebuild_command(args),
            #[cfg(feature = "xz")]
            "pack-matrix" => pack_matrix(args, start),
            #[cfg(feature = "xz")]
            "unpack-matrix" => unpack_matrix(args),
            #[cfg(feature = "xz")]
            "pack-int" => pack_int(args, start),
            #[cfg(feature = "xz")]
            "unpack-int" => unpack_int(args),
            #[cfg(feature = "xz")]
            "check-int" => check_int(args, start),
            "bench-cold" => bench_cold(args),
            "pair-cover" => pair_cover(args),
            #[cfg(feature = "pair-stats")]
            "pair-stats" => pair_stats(args),
            #[cfg(not(feature = "pair-stats"))]
            "pair-stats" => Err(format!(
                "{command}: this build has no pair counting; build with --features pair-stats"
            )),
            "runtime-int" => runtime_int(args),
            "check-runtime-int" => check_runtime_int(args),
            #[cfg(not(feature = "xz"))]
            "pack-int" | "unpack-int" | "check-int" => Err(format!(
                "{command}: this build has no xz support; build with --features xz"
            )),
            #[cfg(not(feature = "xz"))]
            "pack-matrix" | "unpack-matrix" | "pack-transport" | "expand" => Err(format!(
                "{command}: this build has no xz support; build with --features xz"
            )),
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
        #[cfg(feature = "xz")]
        "pack-transport" => mecab_rs::packed::write_transport(input, output)
            .map(|_| ())
            .map_err(shown),
        #[cfg(feature = "xz")]
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
    let default_def = directory.join("char.def");
    let char_def: Option<&Path> = match &char_def {
        Some(file) => Some(file),
        None => default_def.is_file().then_some(default_def.as_path()),
    };
    let output = directory.join("char.uni");
    CharTable::convert(&directory.join("char.bin"), char_def, &output)
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
