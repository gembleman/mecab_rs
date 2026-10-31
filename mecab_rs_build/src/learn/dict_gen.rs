use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::{Config, LineEnding, LineEnds, open_lines};
use crate::build::char_def::CharProperty;
use crate::build::model::CostModel;
use crate::build::rewrite::Rewriter;
use crate::build::text::{LINE_BUFFER_BYTES, Lines, cstr, escape_csv_element, tokenize_csv};
use crate::build::{BuildOptions, compile_system, csv_files};
use crate::connection_templates::to_cost;
use mecab_rs::__private::dictionary_format::{DictionaryKind, SystemDictionary};
use mecab_rs::__private::files::{
    CHAR_BIN_FILE, CHAR_DEF_FILE, DICRC_FILE, FEATURE_DEF_FILE, LEFT_ID_DEF_FILE, MATRIX_DEF_FILE,
    MODEL_DEF_FILE, REWRITE_DEF_FILE, RIGHT_ID_DEF_FILE, SYS_DIC_FILE, UNK_DEF_FILE, write_file,
    write_file_with,
};
use mecab_rs::Error;

#[derive(Clone, Debug)]
pub struct GenerateOptions {
    pub dic_dir: PathBuf,
    pub out_dir: PathBuf,
    pub model: Option<PathBuf>,
    pub crlf: bool,
    pub build: bool,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            dic_dir: PathBuf::from("."),
            out_dir: PathBuf::from("."),
            model: None,
            crlf: false,
            build: false,
        }
    }
}

#[derive(Default)]
struct ContextIds {
    left: BTreeMap<Vec<u8>, i32>,
    right: BTreeMap<Vec<u8>, i32>,
    left_bos: Vec<u8>,
    right_bos: Vec<u8>,
}

impl ContextIds {
    fn build(&mut self) {
        for (map, bos) in [
            (&mut self.left, &self.left_bos),
            (&mut self.right, &self.right_bos),
        ] {
            for (id, value) in (1..).zip(map.values_mut()) {
                *value = id;
            }
            map.entry(bos.clone()).or_insert(0);
        }
    }

    fn save(&self, left: &Path, right: &Path, ending: LineEnding) -> Result<(), Error> {
        for (path, map) in [(left, &self.left), (right, &self.right)] {
            write_file(path, |file| {
                let mut file = LineEnds::new(file, ending);
                for (key, id) in map {
                    write!(file, "{id} ")?;
                    file.write_all(key)?;
                    file.write_all(b"\n")?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

fn progress_bar(
    log: &mut dyn Write,
    message: &str,
    current: usize,
    total: usize,
    previous: &mut i32,
) {
    const BAR: &str = "###########################################";
    let scale = BAR.len() as i32;
    let percentage = (100.0 * current as f64 / total as f64) as i32;
    let bar_len = (1.0 * current as f64 * f64::from(scale) / total as f64) as i32;
    if *previous != percentage {
        let shown = (bar_len.max(0) as usize).min(BAR.len());
        let padding = (scale - bar_len).unsigned_abs() as usize;
        let _ = write!(
            log,
            "{message}: {percentage:3}% |{}{}| ",
            &BAR[..shown],
            " ".repeat(padding)
        );
        let _ = write!(log, "{}", if percentage == 100 { "\n" } else { "\r" });
        let _ = log.flush();
    }
    *previous = percentage;
}

const LEXICON_COLUMNS: usize = 5;
const SURFACE_COLUMN: usize = 0;
const FEATURE_COLUMN: usize = 4;
const NOT_FOUND: i32 = -1;

fn entry(line: &mut [u8], path: &Path) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
    let columns = tokenize_csv(line, LEXICON_COLUMNS);
    if columns.len() != LEXICON_COLUMNS {
        return Err(Error::learn(path, format!("format error: {shown}")));
    }
    Ok((
        cstr(line, columns[SURFACE_COLUMN]).to_vec(),
        cstr(line, columns[FEATURE_COLUMN]).to_vec(),
    ))
}

fn copy(source: &Path, destination: &Path, log: &mut dyn Write) -> Result<(), Error> {
    let _ = writeln!(
        log,
        "copying {} to {}",
        source.display(),
        destination.display()
    );
    std::fs::copy(source, destination)
        .map(|_| ())
        .map_err(super::io_error(source))
}

pub fn generate(options: &GenerateOptions, log: &mut dyn Write) -> Result<(), Error> {
    let dic_dir = options.dic_dir.as_path();
    let out_dir = options.out_dir.as_path();
    let mut config = Config::default();
    config.load(&dic_dir.join(DICRC_FILE))?;

    {
        let system = SystemDictionary::open(&dic_dir.join(SYS_DIC_FILE), DictionaryKind::System)?;
        if system.charset().is_empty() {
            return Err(Error::learn(dic_dir, "the charset of sys.dic is empty"));
        }
    }
    let property = CharProperty::open(&dic_dir.join(CHAR_BIN_FILE))?;

    let bos = config.text("bos-feature").as_bytes().to_vec();
    let factor: i32 = config.number("cost-factor");
    let dictionaries = csv_files(dic_dir)?;

    let same_directory = dic_dir == out_dir
        || std::fs::canonicalize(dic_dir)
            .ok()
            .zip(std::fs::canonicalize(out_dir).ok())
            .is_some_and(|(a, b)| a == b);
    if same_directory {
        return Err(Error::learn(
            dic_dir,
            "output directory = dictionary directory! Please specify different directory.",
        ));
    }
    if out_dir.as_os_str().is_empty() {
        return Err(Error::learn(dic_dir, "output directory is empty"));
    }
    let Some(model_path) = options
        .model
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Err(Error::learn(dic_dir, "model file is empty"));
    };
    let model = CostModel::open(model_path, dic_dir, "", "UTF-8")?;
    if factor <= 0 {
        return Err(Error::learn(
            dic_dir,
            "cost factor needs to be positive value",
        ));
    }
    if bos.is_empty() {
        return Err(Error::learn(dic_dir, "bos-feature is empty"));
    }
    if dictionaries.is_empty() {
        return Err(Error::learn(
            dic_dir,
            format!("no dictionary is found in {}", dic_dir.display()),
        ));
    }
    let rewrite_def = dic_dir.join(REWRITE_DEF_FILE);
    let feature_def = dic_dir.join(FEATURE_DEF_FILE);
    let mut rewriter = Rewriter::open(&rewrite_def)?;
    std::fs::create_dir_all(out_dir).map_err(super::io_error(out_dir))?;

    let rewrite = |rewriter: &mut Rewriter, feature: &[u8]| {
        let (matched, rewritten) = rewriter.rewrite_cached(feature, &rewrite_def)?;
        if matched {
            Ok(rewritten)
        } else {
            Err(Error::learn(
                &rewrite_def,
                format!(
                    "cannot rewrite pattern: {}",
                    String::from_utf8_lossy(feature)
                ),
            ))
        }
    };

    let mut ids = ContextIds::default();
    let bos = rewrite(&mut rewriter, &bos)?;
    ids.left_bos = bos.left;
    ids.right_bos = bos.right;

    let unk_def = dic_dir.join(UNK_DEF_FILE);
    let sources: Vec<&Path> = std::iter::once(unk_def.as_path())
        .chain(dictionaries.iter().map(PathBuf::as_path))
        .collect();
    for source in &sources {
        let mut lines = open_lines(source)?;
        let _ = write!(log, "reading {} ... ", source.display());
        let _ = log.flush();
        let mut count = 0usize;
        while let Some(line) = lines.next_line()? {
            let (_, feature) = entry(line, source)?;
            let rewritten = rewrite(&mut rewriter, &feature)?;
            ids.left.entry(rewritten.left).or_insert(1);
            ids.right.entry(rewritten.right).or_insert(1);
            count += 1;
        }
        let _ = writeln!(log, "{count}");
    }

    let left_id = out_dir.join(LEFT_ID_DEF_FILE);
    let right_id = out_dir.join(RIGHT_ID_DEF_FILE);
    let _ = writeln!(
        log,
        "emitting {}/ {}",
        left_id.display(),
        right_id.display()
    );
    ids.build();
    ids.save(&left_id, &right_id, LineEnding::from(options.crlf))?;

    for (number, source) in sources.iter().enumerate() {
        let unknown = number == 0;
        let output = out_dir.join(source.file_name().unwrap_or_default());
        let mut lines = Lines::open(source, Some(LINE_BUFFER_BYTES))?;
        let _ = write!(log, "emitting {} ... ", output.display());
        let _ = log.flush();
        let mut count = 0usize;
        write_file_with(&output, |file| {
            let mut file = LineEnds::new(file, LineEnding::from(options.crlf));
            let io = super::io_error(&output);
            while let Some(line) = lines.next_line()? {
                let (mut word, feature) = entry(line, source)?;
                let rewritten = rewrite(&mut rewriter, &feature)?;
                let left = ids.left.get(&rewritten.left).copied().unwrap_or(NOT_FOUND);
                let right = ids
                    .right
                    .get(&rewritten.right)
                    .copied()
                    .unwrap_or(NOT_FOUND);
                if left <= 0 {
                    return Err(Error::learn(
                        source,
                        format!(
                            "CID is not found for {}",
                            String::from_utf8_lossy(&rewritten.left)
                        ),
                    ));
                }
                if right <= 0 {
                    return Err(Error::learn(
                        source,
                        format!(
                            "CID is not found for {}",
                            String::from_utf8_lossy(&rewritten.right)
                        ),
                    ));
                }
                let char_type = if unknown {
                    property.id(&word).ok_or_else(|| {
                        Error::learn(
                            source,
                            format!("unknown property [{}]", String::from_utf8_lossy(&word)),
                        )
                    })? as u8
                } else {
                    property.default_type(&word)
                };
                let weight = model.cost(&rewritten.unigram, char_type, &feature_def)?;
                escape_csv_element(&mut word);
                file.write_all(&word).map_err(&io)?;
                write!(
                    file,
                    ",{left},{right},{},",
                    to_cost(weight, f64::from(factor))
                )
                .map_err(&io)?;
                file.write_all(&feature).map_err(&io)?;
                file.write_all(b"\n").map_err(&io)?;
                count += 1;
            }
            Ok(())
        })?;
        let _ = writeln!(log, "{count}");
    }

    let matrix = out_dir.join(MATRIX_DEF_FILE);
    let mut previous = 0;
    write_file_with(&matrix, |file| {
        let mut file = LineEnds::new(file, LineEnding::from(options.crlf));
        let io = super::io_error(&matrix);
        if ids.left.is_empty() {
            return Err(Error::learn(dic_dir, "left id size is empty"));
        }
        if ids.right.is_empty() {
            return Err(Error::learn(dic_dir, "right id size is empty"));
        }
        writeln!(file, "{} {}", ids.right.len(), ids.left.len()).map_err(&io)?;
        let mut row = 0;
        for (right_key, right_id) in &ids.right {
            row += 1;
            progress_bar(
                log,
                "emitting matrix      ",
                row + 1,
                ids.right.len(),
                &mut previous,
            );
            for (left_key, left_id) in &ids.left {
                let weight = model.connection_cost(right_key, left_key, &feature_def)?;
                writeln!(
                    file,
                    "{right_id} {left_id} {}",
                    to_cost(weight, f64::from(factor))
                )
                .map_err(&io)?;
            }
        }
        Ok(())
    })?;

    copy(
        &dic_dir.join(CHAR_DEF_FILE),
        &out_dir.join(CHAR_DEF_FILE),
        log,
    )?;
    copy(&rewrite_def, &out_dir.join(REWRITE_DEF_FILE), log)?;
    copy(&dic_dir.join(DICRC_FILE), &out_dir.join(DICRC_FILE), log)?;
    copy(&feature_def, &out_dir.join(FEATURE_DEF_FILE), log)?;
    copy(model_path, &out_dir.join(MODEL_DEF_FILE), log)?;
    let _ = writeln!(log, "\ndone!");

    if options.build {
        compile_system(out_dir, out_dir, &BuildOptions::default())?;
    }
    Ok(())
}
