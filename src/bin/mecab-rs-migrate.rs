
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mecab_rs::CharTable;

const USAGE: &str = "usage: mecab-rs-migrate <dictionary-directory> [--char-def <file>]

Converts <dictionary-directory>/char.bin into <dictionary-directory>/char.uni. Range lines
above the BMP of char.def (the one in the directory, or the given file) are applied on top of
the built-in defaults; the other files of the directory are not touched.";

fn run() -> Result<(), String> {
    let mut directory: Option<PathBuf> = None;
    let mut char_def: Option<PathBuf> = None;
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--char-def" {
            let file = arguments.next().ok_or("--char-def needs a file")?;
            char_def = Some(file.into());
        } else if argument == "-h" || argument == "--help" {
            println!("{USAGE}");
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
