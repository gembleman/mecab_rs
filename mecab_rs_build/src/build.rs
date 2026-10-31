use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use mecab_rs::__private::dictionary_format::DictionaryKind;
use mecab_rs::__private::files::{
    CHAR_BIN_FILE, CHAR_DEF_FILE, CHAR_TABLE_FILE, DICRC_FILE, MATRIX_BIN_FILE, MATRIX_DEF_FILE,
    MODEL_BIN_FILE, MODEL_DEF_FILE, SYS_DIC_FILE, UNK_DEF_FILE, UNK_DIC_FILE, io_error, write_file,
    write_file_with,
};
use mecab_rs::CharTable;
use mecab_rs::Error;

pub(crate) mod char_def;
mod darts;
mod dictionary;
mod matrix;
pub(crate) mod model;
pub(crate) mod rewrite;
mod settings;
pub(crate) mod text;

pub(crate) use settings::{Charset, Settings};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "one flag per buildable part")]
pub struct Parts {
    pub unknown: bool,
    pub char_category: bool,
    pub model: bool,
    pub system: bool,
    pub matrix: bool,
}

impl Parts {
    pub const ALL: Self = Self {
        unknown: true,
        char_category: true,
        model: true,
        system: true,
        matrix: true,
    };

    fn any(self) -> bool {
        self.unknown || self.char_category || self.model || self.system || self.matrix
    }
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub model: Option<PathBuf>,
    pub dictionary_charset: String,
    pub charset: String,
    pub wakati: bool,
    pub parts: Parts,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            model: None,
            dictionary_charset: "UTF-8".to_owned(),
            charset: "UTF-8".to_owned(),
            wakati: false,
            parts: Parts::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct BuildReport {
    pub read: Vec<(PathBuf, usize)>,
    pub written: Vec<PathBuf>,
}

impl BuildReport {
    fn from_stage(inputs: &[PathBuf], counts: Vec<usize>, written: PathBuf) -> Self {
        Self {
            read: inputs.iter().cloned().zip(counts).collect(),
            written: vec![written],
        }
    }

    fn written_only(written: Vec<PathBuf>) -> Self {
        Self {
            read: Vec::new(),
            written,
        }
    }

    fn absorb(&mut self, stage: Self) {
        self.read.extend(stage.read);
        self.written.extend(stage.written);
    }
}

const MIN_CSV_NAME_LEN: usize = 5;

pub(crate) fn csv_files(dic_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut names = Vec::new();
    let listing = fs::read_dir(dic_dir).map_err(io_error(dic_dir))?;
    for entry in listing {
        let entry = entry.map_err(io_error(dic_dir))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() >= MIN_CSV_NAME_LEN && name.to_ascii_lowercase().ends_with(".csv") {
            names.push(name);
        }
    }
    names.sort_by_key(|name| name.to_ascii_uppercase());
    Ok(names.into_iter().map(|name| dic_dir.join(name)).collect())
}

fn selected_parts(options: &BuildOptions) -> Parts {
    if options.parts.any() {
        options.parts
    } else {
        Parts::ALL
    }
}

pub fn compile_system(
    dic_dir: &Path,
    out_dir: &Path,
    options: &BuildOptions,
) -> Result<BuildReport, Error> {
    let settings = Settings::load(dic_dir, options)?;
    let parts = selected_parts(options);
    fs::create_dir_all(out_dir).map_err(io_error(out_dir))?;
    let mut report = BuildReport::default();

    if parts.char_category || parts.unknown {
        report.absorb(write_char_tables(dic_dir, out_dir)?);
    }
    if parts.unknown {
        report.absorb(write_unknown_dictionary(&settings, out_dir)?);
    }
    if parts.model {
        report.absorb(write_model(&settings, out_dir)?);
    }
    if parts.system {
        report.absorb(write_system_dictionary(&settings, out_dir)?);
    }
    if parts.matrix {
        report.absorb(write_matrix(dic_dir, out_dir)?);
    }

    copy_dicrc(dic_dir, out_dir)?;
    Ok(report)
}

fn write_char_tables(dic_dir: &Path, out_dir: &Path) -> Result<BuildReport, Error> {
    let bin = out_dir.join(CHAR_BIN_FILE);
    let definition = dic_dir.join(CHAR_DEF_FILE);
    let bytes = char_def::compile(&definition, &dic_dir.join(UNK_DEF_FILE))?;
    write_file(&bin, |file| file.write_all(&bytes))?;
    let uni = out_dir.join(CHAR_TABLE_FILE);
    CharTable::convert(
        &bin,
        definition.is_file().then_some(definition.as_path()),
        &uni,
    )?;
    Ok(BuildReport::written_only(vec![bin, uni]))
}

fn write_unknown_dictionary(settings: &Settings, out_dir: &Path) -> Result<BuildReport, Error> {
    let unknown = vec![settings.dic_dir.join(UNK_DEF_FILE)];
    let output = out_dir.join(UNK_DIC_FILE);
    let counts = dictionary::compile(settings, DictionaryKind::Unknown, &unknown, &output)?;
    Ok(BuildReport::from_stage(&unknown, counts, output))
}

fn write_model(settings: &Settings, out_dir: &Path) -> Result<BuildReport, Error> {
    let text = settings.dic_dir.join(MODEL_DEF_FILE);
    if !text.is_file() {
        eprintln!("{} is not found. skipped.", text.display());
        return Ok(BuildReport::default());
    }
    let output = out_dir.join(MODEL_BIN_FILE);
    let bytes = model::convert(
        &text,
        &settings.options.dictionary_charset,
        &settings.options.charset,
    )?;
    write_file(&output, |file| file.write_all(&bytes))?;
    Ok(BuildReport::written_only(vec![output]))
}

fn write_system_dictionary(settings: &Settings, out_dir: &Path) -> Result<BuildReport, Error> {
    let inputs = csv_files(&settings.dic_dir)?;
    if inputs.is_empty() {
        return Err(Error::build(
            &settings.dic_dir,
            "no dictionaries are specified",
        ));
    }
    let output = out_dir.join(SYS_DIC_FILE);
    let counts = dictionary::compile(settings, DictionaryKind::System, &inputs, &output)?;
    Ok(BuildReport::from_stage(&inputs, counts, output))
}

fn write_matrix(dic_dir: &Path, out_dir: &Path) -> Result<BuildReport, Error> {
    let output = out_dir.join(MATRIX_BIN_FILE);
    write_file_with(&output, |file| {
        matrix::compile(&dic_dir.join(MATRIX_DEF_FILE), &output, file)
    })?;
    Ok(BuildReport::written_only(vec![output]))
}

fn copy_dicrc(dic_dir: &Path, out_dir: &Path) -> Result<(), Error> {
    let target = out_dir.join(DICRC_FILE);
    let source = dic_dir.join(DICRC_FILE);
    let same = fs::canonicalize(dic_dir).ok() == fs::canonicalize(out_dir).ok();
    if !same && !target.exists() {
        fs::copy(&source, &target).map_err(io_error(&source))?;
    }
    Ok(())
}

fn require_user_inputs(dic_dir: &Path, csv_files: &[PathBuf]) -> Result<(), Error> {
    if csv_files.is_empty() {
        Err(Error::build(dic_dir, "no dictionaries are specified"))
    } else {
        Ok(())
    }
}

pub fn compile_user(
    dic_dir: &Path,
    csv_files: &[PathBuf],
    output: &Path,
    options: &BuildOptions,
) -> Result<BuildReport, Error> {
    let settings = Settings::load(dic_dir, options)?;
    require_user_inputs(dic_dir, csv_files)?;
    let counts = dictionary::compile(&settings, DictionaryKind::User, csv_files, output)?;
    Ok(BuildReport::from_stage(
        csv_files,
        counts,
        output.to_path_buf(),
    ))
}

pub fn assign_user_costs(
    dic_dir: &Path,
    csv_files: &[PathBuf],
    output: &Path,
    options: &BuildOptions,
) -> Result<BuildReport, Error> {
    let settings = Settings::load(dic_dir, options)?;
    require_user_inputs(dic_dir, csv_files)?;
    let counts = dictionary::assign_costs(&settings, csv_files, output)?;
    Ok(BuildReport::from_stage(
        csv_files,
        counts,
        output.to_path_buf(),
    ))
}
