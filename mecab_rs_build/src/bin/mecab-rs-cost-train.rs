use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use mecab_rs_build::cli_options::{OptionSpec, Options};
use mecab_rs_build::learn::{TrainOptions, train};

const USAGE: &str = "usage: mecab-rs-cost-train [options] corpus model

 -d, --dicdir=DIR      set DIR as dicdir(default \".\" )
 -M, --old-model=FILE  set FILE as old CRF model file
 -c, --cost=FLOAT      set FLOAT for cost C for constraints violatoin
 -f, --freq=INT        set the frequency cut-off (default 1)
 -e, --eta=DIR         set FLOAT for tolerance of termination criterion
 -p, --thread=INT      number of threads(default 1)
     --crlf            write \\r\\n line ends, as the Windows build of MeCab does
 -v, --version         show the version and exit
 -h, --help            show this help and exit.

The corpus is in the output format of mecab (surface<TAB>feature lines, EOS after each
sentence). The dicdir needs dicrc, sys.dic, unk.dic, char.bin, feature.def and rewrite.def.";

const SPECS: [OptionSpec; 9] = [
    OptionSpec {
        name: "dicdir",
        short: Some('d'),
        takes_value: true,
        default: Some("."),
    },
    OptionSpec {
        name: "old-model",
        short: Some('M'),
        takes_value: true,
        default: None,
    },
    OptionSpec {
        name: "cost",
        short: Some('c'),
        takes_value: true,
        default: Some("1.0"),
    },
    OptionSpec {
        name: "freq",
        short: Some('f'),
        takes_value: true,
        default: Some("1"),
    },
    OptionSpec {
        name: "eta",
        short: Some('e'),
        takes_value: true,
        default: Some("0.00005"),
    },
    OptionSpec {
        name: "thread",
        short: Some('p'),
        takes_value: true,
        default: Some("1"),
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

fn run() -> Result<ExitCode, String> {
    let options = Options::from_env(&SPECS)?;
    if options.flag("help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if options.flag("version") {
        println!("mecab-rs-cost-train of {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    let [corpus, model] = options.rest.as_slice() else {
        println!("Usage: mecab-rs-cost-train corpus model");
        return Ok(ExitCode::FAILURE);
    };
    let train_options = TrainOptions {
        dic_dir: PathBuf::from(options.get("dicdir").unwrap_or(".")),
        old_model: options.get("old-model").map(PathBuf::from),
        cost: options.number("cost"),
        freq: options.number("freq"),
        eta: options.number("eta"),
        threads: options.number("thread"),
        crlf: options.flag("crlf"),
    };
    let stdout = std::io::stdout();
    let mut log = stdout.lock();
    let result = train(
        &train_options,
        &PathBuf::from(corpus),
        &PathBuf::from(model),
        &mut log,
    );
    let _ = log.flush();
    result.map_err(|error| error.to_string())?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("mecab-rs-cost-train: {message}\n\ntry '--help' for more information.");
            ExitCode::FAILURE
        }
    }
}
