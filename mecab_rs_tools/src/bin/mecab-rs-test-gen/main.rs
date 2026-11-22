use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_tools::cli_options::{OptionSpec, Options, lossy_arguments};
use mecab_rs_tools::learn::test_sentences;

const USAGE: &str = "usage: mecab-rs-test-gen [options] [files]

 -o, --output=FILE set the output filename
 -v, --version     show the version and exit
 -h, --help        show this help and exit.

Without files it reads standard input. As in MeCab 0.996, which reads its command line twice,
every file named is read twice.";

const SPECS: [OptionSpec; 3] = [
    OptionSpec {
        name: "output",
        short: Some('o'),
        takes_value: true,
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
            &format!("mecab-rs-test-gen of {}", env!("CARGO_PKG_VERSION")),
        );
    }
    let mut inputs: Vec<PathBuf> = options.rest.iter().map(PathBuf::from).collect();
    inputs.extend(inputs.clone());
    let mut writer;
    let out: &mut dyn Write = match options.get("output") {
        None | Some("-") => stdout,
        Some(path) => {
            writer = std::io::BufWriter::new(
                std::fs::File::create(path)
                    .map_err(|error| format!("permission denied: {path}: {error}"))?,
            );
            &mut writer
        }
    };
    test_sentences(&inputs, out).map_err(|error| error.report())?;
    out.flush().map_err(|error| error.to_string())
}

fn run() -> Result<(), String> {
    let arguments = lossy_arguments(&std::env::args_os().skip(1).collect::<Vec<_>>());
    run_with(&arguments, &mut std::io::stdout().lock())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mecab-rs-test-gen: {message}\n\ntry '--help' for more information.");
            ExitCode::FAILURE
        }
    }
}
