use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use mecab_rs::__private::files::{CHAR_BIN_FILE, CHAR_DEF_FILE, CHAR_TABLE_FILE};
use mecab_rs::CharTable;

mod check;
mod matrix_args;
mod rebuild_command;
#[cfg(feature = "pair-stats")]
mod stats;
mod usage;

use usage::{REBUILD_USAGE, USAGE};

#[allow(clippy::needless_pass_by_value, reason = "used as a map_err callback")]
fn shown(error: mecab_rs::Error) -> String {
    error.to_string()
}

fn run() -> Result<(), String> {
    let all: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let Some(command) = all.first().and_then(|first| first.to_str()) {
        let start = Instant::now();
        let args = &all[1..];
        let result = match command {
            "pack" => packed_command(command, args),
            "pack-transport" | "expand" => packed_command(command, args),
            "rebuild" => rebuild_command::rebuild_command(args),
            #[cfg(feature = "pair-stats")]
            "pair-stats" => stats::pair_stats(args),
            #[cfg(not(feature = "pair-stats"))]
            "pair-stats" => Err(format!(
                "{command}: this build has no pair counting; build with --features pair-stats"
            )),
            "check-runtime-int" => check::check_runtime_int(args),
            _ => return migrate_char_table(all),
        };
        result?;
        println!("{command}: {:.2} s", start.elapsed().as_secs_f64());
        return Ok(());
    }
    migrate_char_table(all)
}

fn packed_command(command: &str, args: &[OsString]) -> Result<(), String> {
    if args.len() != 2 {
        return Err(format!("{command}: expected two paths"));
    }
    let input = Path::new(&args[0]);
    let output = Path::new(&args[1]);
    match command {
        "pack" => mecab_rs::packed::write_runtime(input, output).map_err(shown),
        "pack-transport" => mecab_rs_build::transport::write_transport(input, output)
            .map(|_| ())
            .map_err(shown),
        "expand" => mecab_rs::packed::expand(input, output).map_err(shown),
        _ => unreachable!(),
    }
}

fn migrate_char_table(all: Vec<OsString>) -> Result<(), String> {
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
    let default_def = directory.join(CHAR_DEF_FILE);
    let char_def: Option<&Path> = match &char_def {
        Some(file) => Some(file),
        None => default_def.is_file().then_some(default_def.as_path()),
    };
    let output = directory.join(CHAR_TABLE_FILE);
    CharTable::convert(&directory.join(CHAR_BIN_FILE), char_def, &output)
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
