use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_tools::cli_options::{OptionSpec, Options, lossy_arguments};
use mecab_rs_tools::learn::evaluate;

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

fn emit(stdout: &mut dyn Write, text: &str) -> Result<(), String> {
    writeln!(stdout, "{text}").map_err(|error| error.to_string())
}

fn run_with(arguments: &[String], stdout: &mut dyn Write) -> Result<ExitCode, String> {
    let options = Options::parse(arguments, &SPECS)?;
    if options.flag("help") {
        emit(stdout, USAGE)?;
        return Ok(ExitCode::SUCCESS);
    }
    if options.flag("version") {
        emit(
            stdout,
            &format!("mecab-rs-system-eval of {}", env!("CARGO_PKG_VERSION")),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let [system, answer, ..] = options.rest.as_slice() else {
        emit(stdout, "Usage: mecab-rs-system-eval output answer")?;
        return Ok(ExitCode::FAILURE);
    };
    let level = options.get("level").unwrap_or("0 -1");
    let mut file;
    let out: &mut dyn Write = match options.get("output") {
        None | Some("-") => stdout,
        Some(path) => {
            file = std::fs::File::create(path).map_err(|error| format!("{path}: {error}"))?;
            &mut file
        }
    };
    evaluate(&PathBuf::from(system), &PathBuf::from(answer), level, out)
        .map_err(|error| error.report())?;
    out.flush().map_err(|error| error.to_string())?;
    Ok(ExitCode::SUCCESS)
}

fn run() -> Result<ExitCode, String> {
    let arguments = lossy_arguments(&std::env::args_os().skip(1).collect::<Vec<_>>());
    run_with(&arguments, &mut std::io::stdout().lock())
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
