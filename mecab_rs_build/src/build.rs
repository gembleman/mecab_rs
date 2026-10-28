use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use mecab_rs::__private::DictionaryKind;
use mecab_rs::__private::{write_file, write_file_with};
use mecab_rs::CharTable;
use mecab_rs::Error;

pub(crate) mod char_def;
mod darts;
mod dictionary;
mod matrix;
pub(crate) mod model;
pub(crate) mod rewrite;
pub(crate) mod text;

use text::{Lines, failure};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Charset {
    Utf8,
    Other,
}

impl Charset {
    pub(crate) fn decode(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "sjis" | "shift-jis" | "shift_jis" | "cp932" | "euc" | "euc_jp" | "euc-jp"
            | "utf16" | "utf_16" | "utf-16" | "utf16be" | "utf_16be" | "utf-16be" | "utf16le"
            | "utf_16le" | "utf-16le" | "ascii" => Self::Other,
            _ => Self::Utf8,
        }
    }
}

pub(crate) struct Settings {
    pub(crate) dic_dir: PathBuf,
    pub(crate) options: BuildOptions,
    pub(crate) cost_factor: i32,
    config_charset: String,
    model: Option<PathBuf>,
    node_format: bool,
}

fn int_or_zero(text: &[u8]) -> i32 {
    std::str::from_utf8(text)
        .ok()
        .and_then(|text| {
            text.trim_matches(|c: char| c.is_ascii_whitespace())
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

impl Settings {
    fn load(dic_dir: &Path, options: &BuildOptions) -> Result<Self, Error> {
        let dicrc = dic_dir.join("dicrc");
        if !dicrc.is_file() {
            return Err(failure(
                &dicrc,
                format!("no such file or directory: {}", dicrc.display()),
            ));
        }
        let mut lines = Lines::open(&dicrc, None)?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        while let Some(line) = lines.next_line()? {
            let line = text::cstr(line, 0);
            if line.is_empty() || line[0] == b';' || line[0] == b'#' {
                continue;
            }
            let Some((key, value)) = text::key_value(line) else {
                return Err(failure(
                    &dicrc,
                    format!("format error: {}", String::from_utf8_lossy(line)),
                ));
            };
            let already_set = entries.iter().any(|(name, _)| name == key);
            if !already_set {
                entries.push((key.to_vec(), value.to_vec()));
            }
        }
        let get = |key: &str| {
            entries
                .iter()
                .find(|(name, _)| name == key.as_bytes())
                .map(|(_, value)| value.as_slice())
        };
        let text_of = |key: &str| {
            get(key)
                .map(|value| String::from_utf8_lossy(value).into_owned())
                .unwrap_or_default()
        };
        let mut config_charset = text_of("config-charset");
        if config_charset.is_empty() {
            config_charset.clone_from(&options.dictionary_charset);
        }
        let model = options.model.clone().or_else(|| {
            let named = text_of("model");
            (!named.is_empty()).then(|| PathBuf::from(named))
        });
        Ok(Self {
            dic_dir: dic_dir.to_path_buf(),
            options: options.clone(),
            cost_factor: get("cost-factor").map_or(0, int_or_zero),
            config_charset,
            model,
            node_format: get("node-format").is_some_and(|value| !value.is_empty()),
        })
    }

    fn require_supported(&self) -> Result<(), Error> {
        let dicrc = self.dic_dir.join("dicrc");
        if self.options.dictionary_charset.is_empty() {
            return Err(failure(&dicrc, "input dictionary charset is empty"));
        }
        if self.options.charset.is_empty() {
            return Err(failure(&dicrc, "output dictionary charset is empty"));
        }
        for (name, what) in [
            (&self.options.dictionary_charset, "the dictionary charset"),
            (
                &self.options.charset,
                "the charset of the binary dictionary",
            ),
            (&self.config_charset, "config-charset"),
        ] {
            if Charset::decode(name) != Charset::Utf8 {
                return Err(failure(
                    &dicrc,
                    format!(
                        "{what} is {name}, but only UTF-8 is supported: convert the sources to UTF-8 and set config-charset = UTF-8 in dicrc"
                    ),
                ));
            }
        }
        if self.node_format {
            return Err(failure(&dicrc, "node-format (-F) is not supported"));
        }
        Ok(())
    }

    fn require_factor(&self) -> Result<(), Error> {
        if self.cost_factor > 0 {
            Ok(())
        } else {
            Err(failure(
                &self.dic_dir.join("dicrc"),
                "cost factor needs to be positive value: set cost-factor in dicrc",
            ))
        }
    }

    fn open_cost_model(&self) -> Result<(model::CostModel, char_def::CharProperty), Error> {
        let model = self
            .model
            .as_deref()
            .ok_or_else(|| failure(&self.dic_dir, "cannot open: no model file (-m)"))?;
        let cost = model::CostModel::open(
            model,
            &self.dic_dir,
            &self.options.dictionary_charset,
            &self.options.charset,
        )?;
        let property = char_def::CharProperty::open(&self.dic_dir.join("char.bin"))?;
        Ok((cost, property))
    }
}

const MIN_CSV_NAME_LEN: usize = 5;

pub(crate) fn csv_files(dic_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut names = Vec::new();
    let listing = fs::read_dir(dic_dir).map_err(|source| Error::Io {
        path: dic_dir.to_path_buf(),
        source,
    })?;
    for entry in listing {
        let entry = entry.map_err(|source| Error::Io {
            path: dic_dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() >= MIN_CSV_NAME_LEN && name.to_ascii_lowercase().ends_with(".csv") {
            names.push(name);
        }
    }
    names.sort_by_key(|name| name.to_ascii_uppercase());
    Ok(names.into_iter().map(|name| dic_dir.join(name)).collect())
}

fn record_inputs_read(report: &mut BuildReport, inputs: &[PathBuf], counts: Vec<usize>) {
    report.read.extend(inputs.iter().cloned().zip(counts));
}

pub fn compile_system(
    dic_dir: &Path,
    out_dir: &Path,
    options: &BuildOptions,
) -> Result<BuildReport, Error> {
    let settings = Settings::load(dic_dir, options)?;
    let parts = if options.parts.any() {
        options.parts
    } else {
        Parts::ALL
    };
    fs::create_dir_all(out_dir).map_err(|source| Error::Io {
        path: out_dir.to_path_buf(),
        source,
    })?;
    let mut report = BuildReport::default();

    if parts.char_category || parts.unknown {
        write_char_tables(dic_dir, out_dir, &mut report)?;
    }
    if parts.unknown {
        write_unknown_dictionary(&settings, out_dir, &mut report)?;
    }
    if parts.model {
        write_model(&settings, out_dir, &mut report)?;
    }
    if parts.system {
        write_system_dictionary(&settings, out_dir, &mut report)?;
    }
    if parts.matrix {
        write_matrix(dic_dir, out_dir, &mut report)?;
    }

    copy_dicrc(dic_dir, out_dir)?;
    Ok(report)
}

fn write_char_tables(
    dic_dir: &Path,
    out_dir: &Path,
    report: &mut BuildReport,
) -> Result<(), Error> {
    let bin = out_dir.join("char.bin");
    let definition = dic_dir.join("char.def");
    let bytes = char_def::compile(&definition, &dic_dir.join("unk.def"))?;
    write_file(&bin, |file| file.write_all(&bytes))?;
    let uni = out_dir.join("char.uni");
    CharTable::convert(
        &bin,
        definition.is_file().then_some(definition.as_path()),
        &uni,
    )?;
    report.written.extend([bin, uni]);
    Ok(())
}

fn write_unknown_dictionary(
    settings: &Settings,
    out_dir: &Path,
    report: &mut BuildReport,
) -> Result<(), Error> {
    let unknown = vec![settings.dic_dir.join("unk.def")];
    let output = out_dir.join("unk.dic");
    let counts = dictionary::compile(settings, DictionaryKind::Unknown, &unknown, &output)?;
    record_inputs_read(report, &unknown, counts);
    report.written.push(output);
    Ok(())
}

fn write_model(settings: &Settings, out_dir: &Path, report: &mut BuildReport) -> Result<(), Error> {
    let text = settings.dic_dir.join("model.def");
    if !text.is_file() {
        eprintln!("{} is not found. skipped.", text.display());
        return Ok(());
    }
    let output = out_dir.join("model.bin");
    let bytes = model::convert(
        &text,
        &settings.options.dictionary_charset,
        &settings.options.charset,
    )?;
    write_file(&output, |file| file.write_all(&bytes))?;
    report.written.push(output);
    Ok(())
}

fn write_system_dictionary(
    settings: &Settings,
    out_dir: &Path,
    report: &mut BuildReport,
) -> Result<(), Error> {
    let inputs = csv_files(&settings.dic_dir)?;
    if inputs.is_empty() {
        return Err(failure(&settings.dic_dir, "no dictionaries are specified"));
    }
    let output = out_dir.join("sys.dic");
    let counts = dictionary::compile(settings, DictionaryKind::System, &inputs, &output)?;
    record_inputs_read(report, &inputs, counts);
    report.written.push(output);
    Ok(())
}

fn write_matrix(dic_dir: &Path, out_dir: &Path, report: &mut BuildReport) -> Result<(), Error> {
    let output = out_dir.join("matrix.bin");
    write_file_with(&output, |file| {
        matrix::compile(&dic_dir.join("matrix.def"), &output, file)
    })?;
    report.written.push(output);
    Ok(())
}

fn copy_dicrc(dic_dir: &Path, out_dir: &Path) -> Result<(), Error> {
    let target = out_dir.join("dicrc");
    let source = dic_dir.join("dicrc");
    let same = fs::canonicalize(dic_dir).ok() == fs::canonicalize(out_dir).ok();
    if !same && !target.exists() {
        fs::copy(&source, &target).map_err(|error| Error::Io {
            path: source,
            source: error,
        })?;
    }
    Ok(())
}

pub fn compile_user(
    dic_dir: &Path,
    csv_files: &[PathBuf],
    output: &Path,
    options: &BuildOptions,
) -> Result<BuildReport, Error> {
    let settings = Settings::load(dic_dir, options)?;
    if csv_files.is_empty() {
        return Err(failure(dic_dir, "no dictionaries are specified"));
    }
    let counts = dictionary::compile(&settings, DictionaryKind::User, csv_files, output)?;
    let mut report = BuildReport::default();
    record_inputs_read(&mut report, csv_files, counts);
    report.written.push(output.to_path_buf());
    Ok(report)
}

pub fn assign_user_costs(
    dic_dir: &Path,
    csv_files: &[PathBuf],
    output: &Path,
    options: &BuildOptions,
) -> Result<BuildReport, Error> {
    let settings = Settings::load(dic_dir, options)?;
    if csv_files.is_empty() {
        return Err(failure(dic_dir, "no dictionaries are specified"));
    }
    let counts = dictionary::assign_costs(&settings, csv_files, output)?;
    let mut report = BuildReport::default();
    record_inputs_read(&mut report, csv_files, counts);
    report.written.push(output.to_path_buf());
    Ok(report)
}
