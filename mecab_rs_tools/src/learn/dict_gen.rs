use std::io::Write;
use std::iter::once;
use std::path::{Path, PathBuf};

use self::context::ContextIds;
use super::{Config, LineEnding, LineEnds, open_lines};
use crate::build::char_def::CharProperty;
use crate::build::model::CostModel;
use crate::build::rewrite::{Rewriter, Rewritten};
use crate::build::text::{LINE_BUFFER_BYTES, Lines, cstr, escape_csv_element, tokenize_csv};
use crate::build::{BuildOptions, compile_system, csv_files};
use crate::connection_templates::to_cost;
use crate::tool_error::{Position, io_error, learn_error};
use mecab_rs::__private::dictionary_format::{DictionaryKind, SystemDictionary};
use mecab_rs::__private::files::{
    CHAR_BIN_FILE, CHAR_DEF_FILE, DICRC_FILE, FEATURE_DEF_FILE, LEFT_ID_DEF_FILE, MODEL_DEF_FILE,
    REWRITE_DEF_FILE, RIGHT_ID_DEF_FILE, SYS_DIC_FILE, UNK_DEF_FILE, write_file_with,
};
use mecab_rs::Error;
use mecab_rs::IoAction;

mod context;
mod matrix;

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

const LEXICON_COLUMNS: usize = 5;
const SURFACE_COLUMN: usize = 0;
const FEATURE_COLUMN: usize = 4;
const NOT_FOUND: i32 = -1;

fn entry(line: &mut [u8], at: Position) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
    let columns = tokenize_csv(line, LEXICON_COLUMNS);
    if columns.len() != LEXICON_COLUMNS {
        return Err(at.learn(format!("format error: {shown}")));
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
        .map_err(io_error(IoAction::Copy, source))
}

struct Rewrites {
    rewriter: Rewriter,
    path: PathBuf,
}

impl Rewrites {
    fn open(dic_dir: &Path) -> Result<Self, Error> {
        let path = dic_dir.join(REWRITE_DEF_FILE);
        let rewriter = Rewriter::open(&path)?;
        Ok(Self { rewriter, path })
    }

    fn apply(&mut self, feature: &[u8]) -> Result<Rewritten, Error> {
        let (matched, rewritten) = self.rewriter.rewrite_cached(feature, &self.path)?;
        if matched {
            Ok(rewritten)
        } else {
            Err(learn_error(
                &self.path,
                format!(
                    "cannot rewrite pattern: {}",
                    String::from_utf8_lossy(feature)
                ),
            ))
        }
    }
}

struct Generator<'a> {
    dic_dir: &'a Path,
    out_dir: &'a Path,
    model_path: &'a Path,
    ending: LineEnding,
    factor: i32,
    feature_def: PathBuf,
    property: CharProperty,
    model: CostModel,
    rewrites: Rewrites,
    ids: ContextIds,
}

fn check_system_charset(dic_dir: &Path) -> Result<(), Error> {
    let system = SystemDictionary::open(&dic_dir.join(SYS_DIC_FILE), DictionaryKind::System)?;
    if system.charset().is_empty() {
        return Err(learn_error(dic_dir, "the charset of sys.dic is empty"));
    }
    Ok(())
}

fn is_same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(a, b)| a == b)
}

fn check_output_directory(dic_dir: &Path, out_dir: &Path) -> Result<(), Error> {
    if is_same_directory(dic_dir, out_dir) {
        return Err(learn_error(
            dic_dir,
            "output directory = dictionary directory! Please specify different directory.",
        ));
    }
    if out_dir.as_os_str().is_empty() {
        return Err(learn_error(dic_dir, "output directory is empty"));
    }
    Ok(())
}

fn required_model_path<'p>(dic_dir: &Path, model: Option<&'p Path>) -> Result<&'p Path, Error> {
    model
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| learn_error(dic_dir, "model file is empty"))
}

fn check_inputs(
    dic_dir: &Path,
    factor: i32,
    bos_feature: &[u8],
    dictionaries: &[PathBuf],
) -> Result<(), Error> {
    if factor <= 0 {
        return Err(learn_error(
            dic_dir,
            "cost factor needs to be positive value",
        ));
    }
    if bos_feature.is_empty() {
        return Err(learn_error(dic_dir, "bos-feature is empty"));
    }
    if dictionaries.is_empty() {
        return Err(learn_error(
            dic_dir,
            format!("no dictionary is found in {}", dic_dir.display()),
        ));
    }
    Ok(())
}

impl<'a> Generator<'a> {
    fn open(options: &'a GenerateOptions) -> Result<(Self, Vec<PathBuf>), Error> {
        let dic_dir = options.dic_dir.as_path();
        let out_dir = options.out_dir.as_path();
        let mut config = Config::default();
        config.load(&dic_dir.join(DICRC_FILE))?;
        check_system_charset(dic_dir)?;
        let property = CharProperty::open(&dic_dir.join(CHAR_BIN_FILE))?;

        let bos_feature = config.text("bos-feature").as_bytes().to_vec();
        let factor: i32 = config.number("cost-factor");
        let dictionaries = csv_files(dic_dir)?;

        check_output_directory(dic_dir, out_dir)?;
        let model_path = required_model_path(dic_dir, options.model.as_deref())?;
        let model = CostModel::open(model_path, dic_dir, "", "UTF-8")?;
        check_inputs(dic_dir, factor, &bos_feature, &dictionaries)?;

        let mut rewrites = Rewrites::open(dic_dir)?;
        std::fs::create_dir_all(out_dir).map_err(io_error(IoAction::Create, out_dir))?;

        let mut ids = ContextIds::default();
        let bos = rewrites.apply(&bos_feature)?;
        ids.left_bos = bos.left;
        ids.right_bos = bos.right;

        let sources = once(dic_dir.join(UNK_DEF_FILE))
            .chain(dictionaries)
            .collect();
        let generator = Self {
            dic_dir,
            out_dir,
            model_path,
            ending: LineEnding::from(options.crlf),
            factor,
            feature_def: dic_dir.join(FEATURE_DEF_FILE),
            property,
            model,
            rewrites,
            ids,
        };
        Ok((generator, sources))
    }

    fn read_sources(&mut self, sources: &[PathBuf], log: &mut dyn Write) -> Result<(), Error> {
        for source in sources {
            let mut lines = open_lines(source)?;
            let _ = write!(log, "reading {} ... ", source.display());
            let _ = log.flush();
            let mut count = 0usize;
            while let Some((number, line)) = lines.next_line()? {
                let (_, feature) = entry(line, Position::new(source, number))?;
                let rewritten = self.rewrites.apply(&feature)?;
                self.ids.left.entry(rewritten.left).or_insert(1);
                self.ids.right.entry(rewritten.right).or_insert(1);
                count += 1;
            }
            let _ = writeln!(log, "{count}");
        }
        Ok(())
    }

    fn emit_context_ids(&mut self, log: &mut dyn Write) -> Result<(), Error> {
        let left_id = self.out_dir.join(LEFT_ID_DEF_FILE);
        let right_id = self.out_dir.join(RIGHT_ID_DEF_FILE);
        let _ = writeln!(
            log,
            "emitting {}/ {}",
            left_id.display(),
            right_id.display()
        );
        self.ids.build();
        self.ids.save(&left_id, &right_id, self.ending)
    }

    fn emit_lexicon(&mut self, sources: &[PathBuf], log: &mut dyn Write) -> Result<(), Error> {
        for (index, source) in sources.iter().enumerate() {
            self.emit_source(index == 0, source, log)?;
        }
        Ok(())
    }

    fn emit_source(
        &mut self,
        unknown: bool,
        source: &Path,
        log: &mut dyn Write,
    ) -> Result<(), Error> {
        let output = self.out_dir.join(source.file_name().unwrap_or_default());
        let mut lines = Lines::open(source, Some(LINE_BUFFER_BYTES))?;
        let _ = write!(log, "emitting {} ... ", output.display());
        let _ = log.flush();
        let mut count = 0usize;
        write_file_with(&output, |file| -> Result<(), Error> {
            let mut file = LineEnds::new(file, self.ending);
            let io = io_error(IoAction::Write, &output);
            while let Some((number, line)) = lines.next_line()? {
                let at = Position::new(source, number);
                let (mut word, feature) = entry(line, at)?;
                let rewritten = self.rewrites.apply(&feature)?;
                let (left, right) = self.connection_ids(&rewritten, at)?;
                let char_type = self.char_type(unknown, &word, at)?;
                let weight = self
                    .model
                    .cost(&rewritten.unigram, char_type, &self.feature_def)?;
                escape_csv_element(&mut word);
                file.write_all(&word).map_err(&io)?;
                write!(
                    file,
                    ",{left},{right},{},",
                    to_cost(weight, f64::from(self.factor))
                )
                .map_err(&io)?;
                file.write_all(&feature).map_err(&io)?;
                file.write_all(b"\n").map_err(&io)?;
                count += 1;
            }
            Ok(())
        })?;
        let _ = writeln!(log, "{count}");
        Ok(())
    }

    fn connection_ids(&self, rewritten: &Rewritten, at: Position) -> Result<(i32, i32), Error> {
        let left = self
            .ids
            .left
            .get(&rewritten.left)
            .copied()
            .unwrap_or(NOT_FOUND);
        let right = self
            .ids
            .right
            .get(&rewritten.right)
            .copied()
            .unwrap_or(NOT_FOUND);
        if left <= 0 {
            return Err(at.learn(format!(
                "CID is not found for {}",
                String::from_utf8_lossy(&rewritten.left)
            )));
        }
        if right <= 0 {
            return Err(at.learn(format!(
                "CID is not found for {}",
                String::from_utf8_lossy(&rewritten.right)
            )));
        }
        Ok((left, right))
    }

    fn char_type(&self, unknown: bool, word: &[u8], at: Position) -> Result<u8, Error> {
        if !unknown {
            return Ok(self.property.default_type(word));
        }
        let id = self.property.id(word).ok_or_else(|| {
            at.learn(format!(
                "unknown property [{}]",
                String::from_utf8_lossy(word)
            ))
        })?;
        Ok(id as u8)
    }

    fn copy_outputs(&self, log: &mut dyn Write) -> Result<(), Error> {
        copy(
            &self.dic_dir.join(CHAR_DEF_FILE),
            &self.out_dir.join(CHAR_DEF_FILE),
            log,
        )?;
        copy(
            &self.rewrites.path,
            &self.out_dir.join(REWRITE_DEF_FILE),
            log,
        )?;
        copy(
            &self.dic_dir.join(DICRC_FILE),
            &self.out_dir.join(DICRC_FILE),
            log,
        )?;
        copy(&self.feature_def, &self.out_dir.join(FEATURE_DEF_FILE), log)?;
        copy(self.model_path, &self.out_dir.join(MODEL_DEF_FILE), log)
    }
}

pub fn generate(options: &GenerateOptions, log: &mut dyn Write) -> Result<(), Error> {
    let (mut generator, sources) = Generator::open(options)?;
    generator.read_sources(&sources, log)?;
    generator.emit_context_ids(log)?;
    generator.emit_lexicon(&sources, log)?;
    generator.emit_matrix(log)?;
    generator.copy_outputs(log)?;
    let _ = writeln!(log, "\ndone!");

    if options.build {
        let out_dir = options.out_dir.as_path();
        compile_system(out_dir, out_dir, &BuildOptions::default())?;
    }
    Ok(())
}
