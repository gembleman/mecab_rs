use std::ffi::OsString;
use std::time::Instant;

use mecab_rs_build::rebuild;

use crate::matrix_args::{MatrixArgs, positional};
use crate::shown;

const COMMAND: &str = "rebuild";
const POSITIONAL_USAGE: &str = "<unidic-full-dir> <out-dir>";
const NEED_STATS_XOR_CORPUS: &str = "rebuild: give either --stats <dir> or --corpus <file>...";

pub fn rebuild_command(args: &[OsString]) -> Result<(), String> {
    let parsed = MatrixArgs::parse(COMMAND, args)?;
    let [source, out] = positional(COMMAND, &parsed.positional, POSITIONAL_USAGE)?;
    let stats = match (&parsed.stats, parsed.corpora.is_empty()) {
        (Some(directory), true) => rebuild::StatsSource::Directory(directory),
        (None, false) => rebuild::StatsSource::Corpora(&parsed.corpora),
        _ => return Err(NEED_STATS_XOR_CORPUS.into()),
    };
    let options = rebuild::RuntimeOptions {
        source,
        out,
        stats,
        keep: parsed.keep.clone(),
        hot: parsed.hot_shape()?,
        threads: parsed.threads(),
        matrix_bin: parsed.matrix_bin,
    };
    let mut previous = Instant::now();
    let report = rebuild::build_runtime(&options, &mut |step| {
        println!("{step}: {:.2} s", previous.elapsed().as_secs_f64());
        previous = Instant::now();
    })
    .map_err(shown)?;
    println!(
        "built {} x {} ids into {}",
        report.left_size,
        report.right_size,
        out.display()
    );
    if let Some(slim) = report.slim {
        println!(
            "sys.dic features {} -> {} bytes, file {} -> {}",
            slim.old_feature_size, slim.new_feature_size, slim.old_file_size, slim.new_file_size
        );
    }
    Ok(())
}
