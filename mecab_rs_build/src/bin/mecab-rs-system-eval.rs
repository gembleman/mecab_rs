use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_build::cli_options::{OptionSpec, Options};
use mecab_rs_build::learn::evaluate;

const USAGE: &str = "usage: mecab-rs-system-eval [options] output answer

 -l, --level=STR   set level of evaluations (default \"0 -1\")
 -o, --output=FILE set the output file name
 -v, --version     show the version and exit
 -h, --help        show this help and exit.";

const SPECS: [OptionSpec; 4] = [
    OptionSpec {
        name: "level",
        short: Some('l'),
        takes_value: true,
        default: Some("0 -1"),
    },
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

fn run() -> Result<ExitCode, String> {
    let options = Options::from_env(&SPECS)?;
    if options.flag("help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if options.flag("version") {
        println!("mecab-rs-system-eval of {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    let [system, answer, ..] = options.rest.as_slice() else {
        println!("Usage: mecab-rs-system-eval output answer");
        return Ok(ExitCode::FAILURE);
    };
    let level = options.get("level").unwrap_or("0 -1");
    let mut out: Box<dyn Write> = match options.get("output") {
        None | Some("-") => Box::new(std::io::stdout().lock()),
        Some(path) => {
            Box::new(std::fs::File::create(path).map_err(|error| format!("{path}: {error}"))?)
        }
    };
    evaluate(
        &PathBuf::from(system),
        &PathBuf::from(answer),
        level,
        &mut out,
    )
    .map_err(|error| error.report())?;
    out.flush().map_err(|error| error.to_string())?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("mecab-rs-system-eval: {message}\n\ntry '--help' for more information.");
            ExitCode::FAILURE
        }
    }
}
