
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
      matrix.hot into <dict-dir>. The hot block defaults to 7104 x 4992 (clamped).
  mecab-rs-migrate check-templates <source-dir> <matrix.bin> [--id-defs <dir>] [--threads N]
      Compiles the templates like `templates` and compares the cost of every cell with the
      dense matrix.bin; exits with an error at the first mismatch.
  mecab-rs-migrate pack-matrix <source-dir> <out.xz> [--id-defs <dir>]      (feature xz)
  mecab-rs-migrate unpack-matrix <file.xz> <dict-dir> [--hot L R] [--threads N]   (feature xz)
      Packs the compiled templates into one xz file, or writes matrix.tpl and matrix.hot
      from such a file.
  mecab-rs-migrate rebuild <unidic-full-dir> <out-dir> (--stats <dir> | --corpus <file>...)
                         [--keep <fields>] [--hot L R] [--threads N] [--xz] [--matrix-bin]
      One-shot build of the runtime files only (dicrc, char.bin, char.uni, license/, sys.dic,
      unk.dic, left-id.def, right-id.def, right.tsv, left.tsv, matrix.tpl, matrix.hot): renumbers
      by the stats (given, or counted from the corpora), slims with --keep, and compiles the
      templates instead of writing matrix.bin. --xz also writes matrix.xz (feature xz),
      --matrix-bin the permuted dense matrix.bin.";

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
    threads: Option<usize>,
    xz: bool,
    matrix_bin: bool,
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
            Some("--threads") => parsed.threads = Some(number(iterator.next(), "--threads")?),
            Some("--xz") => parsed.xz = true,
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

fn threads_of(parsed: &MatrixArgs) -> usize {
    parsed.threads.unwrap_or_else(rebuild::default_threads)
}

fn templates(args: &Args, start: Instant) -> Result<(), String> {
    let parsed = parse_matrix_args("templates", args)?;
    let [source, dictionary] = positional("templates", &parsed, "<source-dir> <dict-dir>")?;
    let templates = compile(source, parsed.id_defs.as_deref())?;
    println!("compiled in {:.2} s", start.elapsed().as_secs_f64());
    std::fs::create_dir_all(dictionary).map_err(|error| error.to_string())?;
    rebuild::write_matrix_files(&templates, dictionary, parsed.hot, threads_of(&parsed))
        .map_err(shown)
}

fn check_templates_command(args: &Args, start: Instant) -> Result<(), String> {
    let parsed = parse_matrix_args("check-templates", args)?;
    let [source, matrix] = positional("check-templates", &parsed, "<source-dir> <matrix.bin>")?;
    let templates = compile(source, parsed.id_defs.as_deref())?;
    println!("compiled in {:.2} s", start.elapsed().as_secs_f64());
    check_templates(&templates, matrix, threads_of(&parsed))
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
        hot: parsed.hot,
        threads: threads_of(&parsed),
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
    rebuild::xz::unpack(pack, dictionary, parsed.hot, threads_of(&parsed)).map_err(shown)
}

fn run() -> Result<(), String> {
    let all: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(command) = all.first().and_then(|first| first.to_str()) {
        let start = Instant::now();
        let args = &all[1..];
        let result = match command {
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
            #[cfg(not(feature = "xz"))]
            "pack-matrix" | "unpack-matrix" => Err(format!(
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
