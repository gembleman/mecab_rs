use std::path::{Path, PathBuf};

use super::int_matrix::{RUNTIME_SHIFT, exact_int_templates, write_int_files};
use super::templates::compile_templates;
use super::{
    ContextStats, Keep, Renumbering, SlimReport, collect_stats, copy_entries, prepare_output,
};
use mecab_rs::__private::files::{
    CHAR_BIN_FILE, CHAR_DEF_FILE, CHAR_TABLE_FILE, DICRC_FILE, FEATURE_DEF_FILE, LEFT_ID_DEF_FILE,
    MATRIX_BIN_FILE, RIGHT_ID_DEF_FILE,
};
use mecab_rs::__private::matrix::HotShape;
use mecab_rs::{CharTable, Error};

pub enum StatsSource<'a> {
    Directory(&'a Path),
    Corpora(&'a [PathBuf]),
}

pub struct RuntimeOptions<'a> {
    pub source: &'a Path,
    pub out: &'a Path,
    pub stats: StatsSource<'a>,
    pub keep: Option<Keep>,
    pub hot: Option<HotShape>,
    pub threads: usize,
    pub matrix_bin: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeReport {
    pub left_size: usize,
    pub right_size: usize,
    pub slim: Option<SlimReport>,
}

pub fn build_runtime(
    options: &RuntimeOptions,
    progress: &mut dyn FnMut(&str),
) -> Result<RuntimeReport, Error> {
    let RuntimeOptions { source, out, .. } = *options;
    prepare_output(source, out)?;
    let (stats, stats_dir) = match options.stats {
        StatsSource::Directory(directory) => (ContextStats::read_tsv(directory)?, directory),
        StatsSource::Corpora(corpora) => {
            let stats = collect_stats(source, corpora)?;
            progress("stats");
            (stats, source)
        }
    };
    let plan = Renumbering::plan(source, &stats, stats_dir)?;
    progress("plan");

    copy_entries(
        source,
        out,
        Some(&[DICRC_FILE, CHAR_BIN_FILE, "license", FEATURE_DEF_FILE]),
    )?;
    let char_def = source.join(CHAR_DEF_FILE);
    CharTable::convert(
        &source.join(CHAR_BIN_FILE),
        char_def.is_file().then_some(char_def.as_path()),
        &out.join(CHAR_TABLE_FILE),
    )?;
    stats.write_tsv(out)?;
    let slim = plan.write_dictionaries(source, out, options.keep.as_ref())?;
    plan.write_id_defs(out)?;
    progress("sys.dic, unk.dic, id definitions");

    let id_def = |name: &str| {
        plan.id_defs
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, text)| text.as_str())
            .expect("both id definitions are planned")
    };
    let templates = compile_templates(source, id_def(LEFT_ID_DEF_FILE), id_def(RIGHT_ID_DEF_FILE))?;
    progress("compile templates");
    let int = exact_int_templates(&templates, RUNTIME_SHIFT, options.threads, out)?;
    write_int_files(&int, out, options.hot.clone(), options.threads)?;
    progress("matrix.itp, matrix.hot");
    if options.matrix_bin {
        plan.write_matrix(source, out)?;
        progress(MATRIX_BIN_FILE);
    }
    Ok(RuntimeReport {
        left_size: plan.left_size,
        right_size: plan.right_size,
        slim,
    })
}
