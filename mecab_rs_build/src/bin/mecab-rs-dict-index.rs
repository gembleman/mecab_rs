use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_build::build::{self, BuildOptions, BuildReport};
use mecab_rs_build::cli_options::{OptionSpec, Options, lossy_arguments};

const USAGE: &str = "usage: mecab-rs-dict-index [options] [files]

  -d, --dicdir=DIR                  set DIR as dic dir (default \".\")
  -o, --outdir=DIR                  set DIR as output dir (default \".\")
  -m, --model=FILE                  use FILE as model file
  -u, --userdic=FILE                build user dictionary
  -a, --assign-user-dictionary-costs
                                    only assign costs/ids to user dictionary
  -U, --build-unknown               build parameters for unknown words
  -M, --build-model                 build model file
  -C, --build-charcategory          build character category maps
  -s, --build-sysdic                build system dictionary
      --build-matrix                build connection matrix
  -c, --charset=ENC                 make charset of binary dictionary ENC (default UTF-8)
  -t, --charset=ENC                 alias of -c
  -f, --dictionary-charset=ENC      assume charset of input CSVs as ENC (default UTF-8)
  -w, --wakati                      build wakati-gaki only dictionary
  -p, --posid                       assign Part-of-speech id (accepted, no effect)
  -F, --node-format=STR             not supported
  -v, --version                     show the version and exit.
  -h, --help                        show this help and exit.

Only UTF-8 is supported for -c, -t and -f. Without -u the system dictionary is built from the
*.csv files of the dic dir: sys.dic, unk.dic, matrix.bin, char.bin, char.uni and model.bin
(when model.def exists). With -u the CSV files given as arguments are compiled into the user
dictionary FILE.";

struct Arguments {
    dic_dir: PathBuf,
    out_dir: PathBuf,
    user: Option<PathBuf>,
    assign: bool,
    rest: Vec<PathBuf>,
    options: BuildOptions,
}

enum Parsed {
    Run(Box<Arguments>),
    Exit,
}

const fn flag(name: &'static str, short: Option<char>) -> OptionSpec {
    OptionSpec {
        name,
        short,
        takes_value: false,
        default: None,
    }
}

const fn valued(name: &'static str, short: Option<char>) -> OptionSpec {
    OptionSpec {
        name,
        short,
        takes_value: true,
        default: None,
    }
}

const SPECS: [OptionSpec; 18] = [
    valued("dicdir", Some('d')),
    valued("outdir", Some('o')),
    valued("model", Some('m')),
    valued("userdic", Some('u')),
    flag("assign-user-dictionary-costs", Some('a')),
    flag("build-unknown", Some('U')),
    flag("build-model", Some('M')),
    flag("build-charcategory", Some('C')),
    flag("build-sysdic", Some('s')),
    flag("build-matrix", None),
    valued("charset", Some('c')),
    valued("charset", Some('t')),
    valued("dictionary-charset", Some('f')),
    flag("wakati", Some('w')),
    flag("posid", Some('p')),
    valued("node-format", Some('F')),
    flag("version", Some('v')),
    flag("help", Some('h')),
];

fn apply(parsed: &mut Arguments, name: &str, text: &str) -> Result<Option<Parsed>, String> {
    let parts = &mut parsed.options.parts;
    match name {
        "dicdir" => parsed.dic_dir = PathBuf::from(text),
        "outdir" => parsed.out_dir = PathBuf::from(text),
        "model" => parsed.options.model = Some(PathBuf::from(text)),
        "userdic" => parsed.user = Some(PathBuf::from(text)),
        "assign-user-dictionary-costs" => parsed.assign = true,
        "build-unknown" => parts.unknown = true,
        "build-model" => parts.model = true,
        "build-charcategory" => parts.char_category = true,
        "build-sysdic" => parts.system = true,
        "build-matrix" => parts.matrix = true,
        "charset" => text.clone_into(&mut parsed.options.charset),
        "dictionary-charset" => text.clone_into(&mut parsed.options.dictionary_charset),
        "wakati" => parsed.options.wakati = true,
        "posid" => {}
        "node-format" => return Err("-F (node-format) is not supported".to_owned()),
        "version" => {
            println!("mecab-rs-dict-index of {}", env!("CARGO_PKG_VERSION"));
            return Ok(Some(Parsed::Exit));
        }
        _ => {
            println!("{USAGE}");
            return Ok(Some(Parsed::Exit));
        }
    }
    Ok(None)
}

fn parse(arguments: &[OsString]) -> Result<Parsed, String> {
    let options = Options::parse(&lossy_arguments(arguments), &SPECS)?;
    let mut parsed = Arguments {
        dic_dir: PathBuf::from("."),
        out_dir: PathBuf::from("."),
        user: None,
        assign: false,
        rest: options.rest.iter().map(PathBuf::from).collect(),
        options: BuildOptions::default(),
    };
    for occurrence in options.occurrences() {
        if let Some(exit) = apply(&mut parsed, occurrence.name, occurrence.value)? {
            return Ok(exit);
        }
    }
    Ok(Parsed::Run(Box::new(parsed)))
}

fn print_report(report: &BuildReport) {
    for (path, count) in &report.read {
        println!("reading {} ... {count}", path.display());
    }
    for path in &report.written {
        println!("wrote {}", path.display());
    }
}

fn run() -> Result<(), String> {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Parsed::Run(arguments) = parse(&arguments)? else {
        return Ok(());
    };
    let shown = |error: mecab_rs::Error| error.report();
    let report = match &arguments.user {
        Some(output) if arguments.assign => build::assign_user_costs(
            &arguments.dic_dir,
            &arguments.rest,
            output,
            &arguments.options,
        )
        .map_err(shown)?,
        Some(output) => build::compile_user(
            &arguments.dic_dir,
            &arguments.rest,
            output,
            &arguments.options,
        )
        .map_err(shown)?,
        None => build::compile_system(&arguments.dic_dir, &arguments.out_dir, &arguments.options)
            .map_err(shown)?,
    };
    print_report(&report);
    println!("\ndone!");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mecab-rs-dict-index: {message}\n\ntry '--help' for more information.");
            ExitCode::FAILURE
        }
    }
}
