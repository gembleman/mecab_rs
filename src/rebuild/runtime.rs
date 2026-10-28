use std::path::{Path, PathBuf};

use super::int_matrix::{exact_int_templates, write_int_files};
use super::templates::{compile_templates, write_matrix_files};
use super::{
    ContextStats, Keep, Renumbering, SlimReport, collect_stats, copy_entries, prepare_output,
};
use crate::{CharTable, Error, HotShape};

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
    pub int_shift: Option<u32>,
    pub xz: bool,
    pub matrix_bin: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeReport {
    pub left_size: usize,
    pub right_size: usize,
    pub slim: Option<SlimReport>,
    pub xz_size: Option<u64>,
}

pub fn build_runtime(
    options: &RuntimeOptions,
    progress: &mut dyn FnMut(&str),
) -> Result<RuntimeReport, Error> {
    let RuntimeOptions { source, out, .. } = *options;
    prepare_output(source, out)?;
    #[cfg(not(feature = "xz"))]
    if options.xz {
        return Err(Error::Unsupported("matrix.xz needs the `xz` cargo feature"));
    }
    let (stats, stats_dir) = match options.stats {
        StatsSource::Directory(directory) => (ContextStats::read_tsv(directory)?, directory),
        StatsSource::Corpora(corpora) => {
            let stats = collect_stats(source, corpora)?;
            progress("stats");
            (stats, source)
        }
    };
    let plan = Renumbering::plan(source, &stats, stats_dir, None)?;
    progress("plan");

    copy_entries(
        source,
        out,
        Some(&["dicrc", "char.bin", "license", "feature.def"]),
    )?;
    let char_def = source.join("char.def");
    CharTable::convert(
        &source.join("char.bin"),
        char_def.is_file().then_some(char_def.as_path()),
        &out.join("char.uni"),
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
    let templates = compile_templates(source, id_def("left-id.def"), id_def("right-id.def"))?;
    progress("compile templates");
    if let Some(shift) = options.int_shift {
        let int = exact_int_templates(&templates, shift, options.threads, out)?;
        write_int_files(&int, out, options.hot.clone(), options.threads)?;
        progress("matrix.itp, matrix.hot");
    } else {
        write_matrix_files(&templates, out, options.hot.clone(), options.threads)?;
        progress("matrix.tpl, matrix.hot");
    }

    #[cfg_attr(not(feature = "xz"), allow(unused_mut))]
    let mut xz_size = None;
    #[cfg(feature = "xz")]
    if options.xz {
        xz_size = Some(super::xz::pack(&templates, &out.join("matrix.xz"))?);
        progress("matrix.xz");
    }
    if options.matrix_bin {
        plan.write_matrix(source, out)?;
        progress("matrix.bin");
    }
    Ok(RuntimeReport {
        left_size: plan.left_size,
        right_size: plan.right_size,
        slim,
        xz_size,
    })
}
