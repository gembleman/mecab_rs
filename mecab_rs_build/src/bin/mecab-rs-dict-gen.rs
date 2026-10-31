use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_build::cli_options::{OptionSpec, Options};
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

fn run() -> Result<(), String> {
    let options = Options::from_env(&SPECS)?;
    if options.flag("help") {
        println!("{USAGE}");
        return Ok(());
    }
    if options.flag("version") {
        println!("mecab-rs-dict-gen of {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let generate_options = GenerateOptions {
        dic_dir: PathBuf::from(options.get("dicdir").unwrap_or(".")),
        out_dir: PathBuf::from(options.get("outdir").unwrap_or(".")),
        model: options.get("model").map(PathBuf::from),
        crlf: options.flag("crlf"),
        build: options.flag("build"),
    };
    let stdout = std::io::stdout();
    let mut log = stdout.lock();
    let result = generate(&generate_options, &mut log);
    let _ = log.flush();
    result.map_err(|error| error.report())
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
