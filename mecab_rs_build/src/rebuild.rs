use std::fs;
use std::path::Path;

use mecab_rs::__private::files::io_error;
use mecab_rs::__private::in_range_bytes::u32_at;
use mecab_rs::Error;

mod hot_pairs;
pub mod int_matrix;
mod renumber;
mod runtime;
mod slim;
mod stats;
mod templates;

pub use hot_pairs::{PAIR_COUNT_BYTES, PairCount, hot_shape_from_pair_counts};
pub use mecab_rs::__private::support::default_threads;
pub(crate) use renumber::Renumbering;
pub use runtime::{RuntimeOptions, RuntimeReport, StatsSource, build_runtime};
pub use slim::{Keep, SlimReport};
pub use stats::{ContextStats, collect_stats};
pub use templates::{DEFAULT_HOT_CELLS, compile_templates};

pub const UNIDIC_FEATURE_COUNT: usize = 29;

const HEADER_WORD_BYTES: usize = 4;

fn header_word(bytes: &[u8], index: usize) -> u32 {
    u32_at(bytes, index * HEADER_WORD_BYTES)
}

fn read_file(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(io_error(path))
}

fn copy_entries(source: &Path, target: &Path, only: Option<&[&str]>) -> Result<(), Error> {
    fs::create_dir_all(target).map_err(io_error(target))?;
    for entry in fs::read_dir(source).map_err(io_error(source))? {
        let entry = entry.map_err(io_error(source))?;
        let name = entry.file_name();
        if only.is_some_and(|wanted| !wanted.iter().any(|wanted| name == *wanted)) {
            continue;
        }
        let from = entry.path();
        let to = target.join(&name);
        if from.is_dir() {
            copy_entries(&from, &to, None)?;
        } else {
            fs::copy(&from, &to).map_err(io_error(&from))?;
        }
    }
    Ok(())
}

fn prepare_output(dict_dir: &Path, out_dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(out_dir).map_err(io_error(out_dir))?;
    let same = fs::canonicalize(dict_dir).map_err(io_error(dict_dir))?
        == fs::canonicalize(out_dir).map_err(io_error(out_dir))?;
    if same {
        return Err(Error::rebuild(
            out_dir,
            "the output directory must differ from the source",
        ));
    }
    Ok(())
}
