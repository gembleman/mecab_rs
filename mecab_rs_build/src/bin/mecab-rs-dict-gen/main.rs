use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_build::cli_options::{OptionSpec, Options, lossy_arguments};
use mecab_rs_build::learn::{GenerateOptions, generate};

const USAGE: &str = "usage: mecab-rs-dict-gen [options]

 -d, --dicdir=DIR  set DIR as dicdir(default \".\" )
 -o, --outdir=DIR  set DIR as output dir
 -m, --model=FILE  use FILE as model file
 -b, --build       also compile the sources written, in the output dir, with the
                   mecab-rs-dict-index code
     --crlf        write \\r\\n line ends, as the Windows build of MeCab does
 -v, --version     show the version and exit
 -h, --help        show this help and exit.

The dicdir needs dicrc, sys.dic, char.bin, char.def, unk.def, the lexicon CSV files,
feature.def and rewrite.def.";

const SPECS: [OptionSpec; 7] = [
    OptionSpec {
        name: "dicdir",
        short: Some('d'),
        takes_value: true,
        default: Some("."),
    },
    OptionSpec {
        name: "outdir",
        short: Some('o'),
        takes_value: true,
        default: Some("."),
    },
    OptionSpec {
        name: "model",
        short: Some('m'),
        takes_value: true,
        default: None,
    },
    OptionSpec {
        name: "build",
        short: Some('b'),
        takes_value: false,
        default: None,
    },
    OptionSpec {
        name: "crlf",
        short: None,
        takes_value: false,
        default: None,
    },
    OptionSpec {
        name: "version",
        short: Some('v'),
        takes_value: false,
        default: None,
    },
    OptionSpec {
        name: "help",
        short: Some('h'),
        takes_value: false,
        default: None,
    },
];

fn emit(stdout: &mut dyn Write, text: &str) -> Result<(), String> {
    writeln!(stdout, "{text}").map_err(|error| error.to_string())
}

fn run_with(arguments: &[String], stdout: &mut dyn Write) -> Result<(), String> {
    let options = Options::parse(arguments, &SPECS)?;
    if options.flag("help") {
        return emit(stdout, USAGE);
    }
    if options.flag("version") {
        return emit(
            stdout,
            &format!("mecab-rs-dict-gen of {}", env!("CARGO_PKG_VERSION")),
        );
    }
    let generate_options = GenerateOptions {
        dic_dir: PathBuf::from(options.get("dicdir").unwrap_or(".")),
        out_dir: PathBuf::from(options.get("outdir").unwrap_or(".")),
        model: options.get("model").map(PathBuf::from),
        crlf: options.flag("crlf"),
        build: options.flag("build"),
    };
    let result = generate(&generate_options, stdout);
    let _ = stdout.flush();
    result.map_err(|error| error.report())
}

fn run() -> Result<(), String> {
    let arguments = lossy_arguments(&std::env::args_os().skip(1).collect::<Vec<_>>());
    run_with(&arguments, &mut std::io::stdout().lock())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mecab-rs-dict-gen: {message}\n\ntry '--help' for more information.");
            ExitCode::FAILURE
        }
    }
}
