use std::path::{Path, PathBuf};

use crate::tool_error::{Position, build_error};
use mecab_rs::__private::files::{CHAR_BIN_FILE, DICRC_FILE};
use mecab_rs::Error;

use super::text::{self, Lines};
use super::{BuildOptions, char_def, model};

const COMMENT_MARKS: &[u8] = b";#";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Charset {
    Utf8,
    Other,
}

impl Charset {
    pub(crate) fn parse(name: &str) -> Result<Self, String> {
        match name.to_ascii_lowercase().as_str() {
            "" | "utf8" | "utf_8" | "utf-8" => Ok(Self::Utf8),
            "sjis" | "shift-jis" | "shift_jis" | "cp932" | "euc" | "euc_jp" | "euc-jp"
            | "utf16" | "utf_16" | "utf-16" | "utf16be" | "utf_16be" | "utf-16be" | "utf16le"
            | "utf_16le" | "utf-16le" | "ascii" => Ok(Self::Other),
            _ => Err(format!("unknown charset: {name}")),
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

struct DicrcEntries(Vec<(Vec<u8>, Vec<u8>)>);

impl DicrcEntries {
    fn read(dicrc: &Path) -> Result<Self, Error> {
        if !dicrc.is_file() {
            return Err(build_error(
                dicrc,
                format!("no such file or directory: {}", dicrc.display()),
            ));
        }
        let mut lines = Lines::open(dicrc, None)?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        while let Some((number, line)) = lines.next_line()? {
            let line = text::cstr(line, 0);
            if line.is_empty() || COMMENT_MARKS.contains(&line[0]) {
                continue;
            }
            let Some((key, value)) = text::key_value(line) else {
                return Err(Position::new(dicrc, number)
                    .build(format!("format error: {}", String::from_utf8_lossy(line))));
            };
            let already_set = entries.iter().any(|(name, _)| name == key);
            if !already_set {
                entries.push((key.to_vec(), value.to_vec()));
            }
        }
        Ok(Self(entries))
    }

    fn get(&self, key: &str) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|(name, _)| name == key.as_bytes())
            .map(|(_, value)| value.as_slice())
    }

    fn text_of(&self, key: &str) -> String {
        self.get(key)
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .unwrap_or_default()
    }
}

impl Settings {
    pub(super) fn load(dic_dir: &Path, options: &BuildOptions) -> Result<Self, Error> {
        let entries = DicrcEntries::read(&dic_dir.join(DICRC_FILE))?;
        let mut config_charset = entries.text_of("config-charset");
        if config_charset.is_empty() {
            config_charset.clone_from(&options.dictionary_charset);
        }
        let model = options.model.clone().or_else(|| {
            let named = entries.text_of("model");
            (!named.is_empty()).then(|| PathBuf::from(named))
        });
        Ok(Self {
            dic_dir: dic_dir.to_path_buf(),
            options: options.clone(),
            cost_factor: entries.get("cost-factor").map_or(0, int_or_zero),
            config_charset,
            model,
            node_format: entries
                .get("node-format")
                .is_some_and(|value| !value.is_empty()),
        })
    }

    pub(super) fn require_supported(&self) -> Result<(), Error> {
        let dicrc = self.dic_dir.join(DICRC_FILE);
        if self.options.dictionary_charset.is_empty() {
            return Err(build_error(&dicrc, "input dictionary charset is empty"));
        }
        if self.options.charset.is_empty() {
            return Err(build_error(&dicrc, "output dictionary charset is empty"));
        }
        for (name, what) in [
            (&self.options.dictionary_charset, "the dictionary charset"),
            (
                &self.options.charset,
                "the charset of the binary dictionary",
            ),
            (&self.config_charset, "config-charset"),
        ] {
            let charset = Charset::parse(name).map_err(|reason| build_error(&dicrc, reason))?;
            if charset != Charset::Utf8 {
                return Err(build_error(
                    &dicrc,
                    format!(
                        "{what} is {name}, but only UTF-8 is supported: convert the sources to UTF-8 and set config-charset = UTF-8 in dicrc"
                    ),
                ));
            }
        }
        if self.node_format {
            return Err(build_error(&dicrc, "node-format (-F) is not supported"));
        }
        Ok(())
    }

    pub(super) fn require_factor(&self) -> Result<(), Error> {
        if self.cost_factor > 0 {
            Ok(())
        } else {
            Err(build_error(
                &self.dic_dir.join(DICRC_FILE),
                "cost factor needs to be positive value: set cost-factor in dicrc",
            ))
        }
    }

    pub(super) fn open_cost_model(
        &self,
    ) -> Result<(model::CostModel, char_def::CharProperty), Error> {
        let model = self
            .model
            .as_deref()
            .ok_or_else(|| build_error(&self.dic_dir, "cannot open: no model file (-m)"))?;
        let cost = model::CostModel::open(
            model,
            &self.dic_dir,
            &self.options.dictionary_charset,
            &self.options.charset,
        )?;
        let property = char_def::CharProperty::open(&self.dic_dir.join(CHAR_BIN_FILE))?;
        Ok((cost, property))
    }
}
