use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_build::cli_options::{OptionSpec, Options};
use mecab_rs_build::learn::test_sentences;

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

fn run() -> Result<(), String> {
    let options = Options::from_env(&SPECS)?;
    if options.flag("help") {
        println!("{USAGE}");
        return Ok(());
    }
    if options.flag("version") {
        println!("mecab-rs-test-gen of {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut inputs: Vec<PathBuf> = options.rest.iter().map(PathBuf::from).collect();
    inputs.extend(inputs.clone());
    let mut out: Box<dyn Write> = match options.get("output") {
        None | Some("-") => Box::new(std::io::stdout().lock()),
        Some(path) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(path)
                .map_err(|error| format!("permission denied: {path}: {error}"))?,
        )),
    };
    test_sentences(&inputs, &mut out).map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())
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
