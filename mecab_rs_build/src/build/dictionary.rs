use std::io::Write;
use std::path::{Path, PathBuf};

use super::Settings;
use super::char_def::{CharProperty, UNK_DEF_DEFAULT};
use super::darts;
use super::matrix::{self, MatrixSize};
use super::model::CostModel;
use super::rewrite::{ContextIds, PosIds, Rewriter};
use super::text::{LINE_BUFFER_BYTES, Lines, atoi, cstr, escape_csv_element, tokenize_csv};
use crate::connection_templates::to_cost;
use mecab_rs::__private::dictionary_format::{
    CHARSET_BYTES, DIC_VERSION, DictionaryKind, HEADER_SIZE, MAGIC_ID, TOKEN_SIZE, VALUE_SIZE_BITS,
};
use mecab_rs::__private::files::{
    FEATURE_DEF_FILE, LEFT_ID_DEF_FILE, REWRITE_DEF_FILE, RIGHT_ID_DEF_FILE, write_file,
};
use mecab_rs::Error;

const CSV_COLUMNS: usize = 5;
const SURFACE_COLUMN: usize = 0;
const LEFT_ID_COLUMN: usize = 1;
const RIGHT_ID_COLUMN: usize = 2;
const COST_COLUMN: usize = 3;
const FEATURE_COLUMN: usize = 4;
const NO_VALUE: i32 = i32::MAX;

struct Entry {
    surface: Vec<u8>,
    left_id: u16,
    right_id: u16,
    pos_id: u16,
    cost: i16,
    feature: u32,
}

fn to_int(text: &[u8]) -> i32 {
    if text.is_empty() {
        NO_VALUE
    } else {
        atoi(text)
    }
}

struct Resources<'a> {
    settings: &'a Settings,
    rewriter: Option<Rewriter>,
    cost: Option<(CostModel, CharProperty)>,
    ids: Option<ContextIds>,
    sizes: MatrixSize,
}

impl<'a> Resources<'a> {
    fn new(settings: &'a Settings) -> Result<Self, Error> {
        Ok(Self {
            settings,
            rewriter: None,
            cost: None,
            ids: None,
            sizes: matrix::sizes(&settings.dic_dir)?,
        })
    }

    fn rewrite_def(&self) -> PathBuf {
        self.settings.dic_dir.join(REWRITE_DEF_FILE)
    }

    fn open_cost_model(&mut self) -> Result<(), Error> {
        if self.rewriter.is_none() {
            self.rewriter = Some(Rewriter::open(&self.rewrite_def())?);
            self.cost = Some(self.settings.open_cost_model()?);
        }
        Ok(())
    }

    fn calc_cost(&mut self, word: &[u8], feature: &[u8]) -> Result<i32, Error> {
        let rewrite_def = self.rewrite_def();
        let (Some(rewriter), Some((model, property))) = (self.rewriter.as_mut(), &self.cost) else {
            return Err(Error::build(
                &self.settings.dic_dir,
                "cannot open feature index",
            ));
        };
        let (_, rewritten) = rewriter.rewrite_cached(feature, &rewrite_def)?;
        let weight = model.cost(
            &rewritten.unigram,
            property.default_type(word),
            &self.settings.dic_dir.join(FEATURE_DEF_FILE),
        )?;
        Ok(i32::from(to_cost(
            weight,
            f64::from(self.settings.cost_factor),
        )))
    }

    fn open_ids(&mut self) -> Result<(), Error> {
        if self.ids.is_none() {
            let dir = &self.settings.dic_dir;
            let ids = ContextIds::open(&dir.join(LEFT_ID_DEF_FILE), &dir.join(RIGHT_ID_DEF_FILE))?;
            if ids.left_size() != self.sizes.left || ids.right_size() != self.sizes.right {
                return Err(Error::build(
                    dir,
                    format!(
                        "Context ID files(left-id.def or right-id.def may be broken: {} {} {} {}",
                        ids.left_size(),
                        self.sizes.left,
                        ids.right_size(),
                        self.sizes.right
                    ),
                ));
            }
            self.ids = Some(ids);
        }
        Ok(())
    }

    fn ids_of(&mut self, feature: &[u8]) -> Result<(i32, i32), Error> {
        if self.rewriter.is_none() {
            self.rewriter = Some(Rewriter::open(&self.rewrite_def())?);
        }
        let path = self.rewrite_def();
        let rewriter = self.rewriter.as_ref().expect("opened above");
        let (matched, rewritten) = rewriter.rewrite(feature, &path)?;
        if !matched {
            return Err(Error::build(
                &path,
                format!("rewrite failed: {}", String::from_utf8_lossy(feature)),
            ));
        }
        self.open_ids()?;
        let ids = self.ids.as_ref().expect("opened above");
        Ok((
            ids.left_id(&rewritten.left)?,
            ids.right_id(&rewritten.right)?,
        ))
    }

    fn ids_are_valid(&self, left_id: i32, right_id: i32) -> bool {
        let left_id_limit = self.sizes.right;
        let right_id_limit = self.sizes.left;
        left_id >= 0
            && right_id >= 0
            && (left_id as usize) < left_id_limit
            && (right_id as usize) < right_id_limit
    }
}

const NO_POS_ID: i32 = -1;

fn invalid_ids_error(left: i32, right: i32, dir: &Path) -> Error {
    Error::build(dir, format!("invalid ids are found lid={left} rid={right}"))
}

struct ReadEntries {
    entries: Vec<Entry>,
    features: Vec<u8>,
    counts: Vec<usize>,
}

fn read_entries(
    settings: &Settings,
    kind: DictionaryKind,
    inputs: &[PathBuf],
    resources: &mut Resources,
    pos_ids: &PosIds,
) -> Result<ReadEntries, Error> {
    let dir = &settings.dic_dir;
    let mut entries: Vec<Entry> = Vec::new();
    let mut features: Vec<u8> = Vec::new();
    let mut counts = Vec::new();
    for input in inputs {
        let mut lines = if kind == DictionaryKind::Unknown && !input.is_file() {
            eprintln!("{} is not found. minimum setting is used.", input.display());
            Lines::builtin(UNK_DEF_DEFAULT, input, Some(LINE_BUFFER_BYTES))
        } else {
            Lines::open(input, Some(LINE_BUFFER_BYTES))?
        };
        let mut count = 0;
        while let Some(line) = lines.next_line()? {
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            let columns = tokenize_csv(line, CSV_COLUMNS);
            if columns.len() != CSV_COLUMNS {
                return Err(Error::build(input, format!("format error: {shown}")));
            }
            let word = cstr(line, columns[SURFACE_COLUMN]).to_vec();
            let mut left = to_int(cstr(line, columns[LEFT_ID_COLUMN]));
            let mut right = to_int(cstr(line, columns[RIGHT_ID_COLUMN]));
            let mut cost = to_int(cstr(line, columns[COST_COLUMN]));
            let feature = cstr(line, columns[FEATURE_COLUMN]).to_vec();
            let pos_id = pos_ids.id(&feature)?;
            if pos_id < NO_POS_ID || pos_id > i32::from(u16::MAX) {
                return Err(Error::build(
                    input,
                    format!("pos id out of range: {pos_id}"),
                ));
            }

            if cost == NO_VALUE {
                if kind != DictionaryKind::User {
                    return Err(Error::build(
                        input,
                        "cost field should not be empty in sys/unk dic.",
                    ));
                }
                resources.open_cost_model()?;
                cost = resources.calc_cost(&word, &feature)?;
            }
            if left < 0 || right < 0 || left == NO_VALUE || right == NO_VALUE {
                (left, right) = resources.ids_of(&feature)?;
            }
            if !resources.ids_are_valid(left, right) {
                return Err(invalid_ids_error(left, right, dir));
            }
            if i16::try_from(cost).is_err() {
                return Err(Error::build(
                    input,
                    format!("cost out of the 16-bit range: {cost}: {shown}"),
                ));
            }
            if word.is_empty() {
                eprintln!("empty word is found, discard this line");
                continue;
            }

            let feature_offset = features.len();
            if !settings.options.wakati {
                if str::from_utf8(&feature).is_err() {
                    return Err(Error::build(
                        input,
                        format!("the feature is not valid UTF-8: {shown}"),
                    ));
                }
                features.extend_from_slice(&feature);
                features.push(0);
            }
            entries.push(Entry {
                surface: word,
                left_id: left as u16,
                right_id: right as u16,
                pos_id: pos_id as u16,
                cost: cost as i16,
                feature: u32::try_from(feature_offset)
                    .map_err(|_| Error::build(input, "the features are longer than 4 GiB"))?,
            });
            count += 1;
        }
        counts.push(count);
    }
    Ok(ReadEntries {
        entries,
        features,
        counts,
    })
}

struct SurfaceGroups<'a> {
    keys: Vec<&'a [u8]>,
    values: Vec<i32>,
}

fn group_by_surface<'a>(entries: &'a [Entry], output: &Path) -> Result<SurfaceGroups<'a>, Error> {
    let mut keys: Vec<&[u8]> = Vec::new();
    let mut values: Vec<i32> = Vec::new();
    let mut push_group = |first_entry: usize, size: usize| -> Result<(), Error> {
        keys.push(&entries[first_entry].surface);
        values.push(
            i32::try_from(size + (first_entry << VALUE_SIZE_BITS)).map_err(|_| {
                Error::build(output, "too many entries for the double array values")
            })?,
        );
        Ok(())
    };
    let mut group_size = 0usize;
    let mut group_start = 0usize;
    for index in 0..entries.len() {
        if index != 0 && entries[index - 1].surface != entries[index].surface {
            push_group(group_start, group_size)?;
            group_size = 1;
            group_start = index;
        } else {
            group_size += 1;
        }
    }
    push_group(group_start, group_size)?;
    Ok(SurfaceGroups { keys, values })
}

fn encode_tokens(entries: &[Entry]) -> Vec<u8> {
    let mut tokens = Vec::with_capacity(entries.len() * TOKEN_SIZE);
    for entry in entries {
        tokens.extend_from_slice(&entry.left_id.to_le_bytes());
        tokens.extend_from_slice(&entry.right_id.to_le_bytes());
        tokens.extend_from_slice(&entry.pos_id.to_le_bytes());
        tokens.extend_from_slice(&entry.cost.to_le_bytes());
        tokens.extend_from_slice(&entry.feature.to_le_bytes());
        tokens.extend_from_slice(&0u32.to_le_bytes());
    }
    tokens
}

struct SectionLengths {
    entries: usize,
    array: usize,
    tokens: usize,
    features: usize,
}

fn encode_header(
    settings: &Settings,
    kind: DictionaryKind,
    sizes: MatrixSize,
    lengths: &SectionLengths,
    output: &Path,
) -> Result<Vec<u8>, Error> {
    let as_u32 = |value: usize, what: &str| {
        u32::try_from(value)
            .map_err(|_| Error::build(output, format!("{what} is longer than 4 GiB")))
    };
    let total = HEADER_SIZE + lengths.array + lengths.tokens + lengths.features;
    let mut header = Vec::with_capacity(HEADER_SIZE);
    for word in [
        as_u32(total, "the dictionary")? ^ MAGIC_ID,
        DIC_VERSION,
        kind as u32,
        as_u32(lengths.entries, "the number of entries")?,
        as_u32(sizes.left, "the left id count")?,
        as_u32(sizes.right, "the right id count")?,
        as_u32(lengths.array, "the double array")?,
        as_u32(lengths.tokens, "the tokens")?,
        as_u32(lengths.features, "the features")?,
        0,
    ] {
        header.extend_from_slice(&word.to_le_bytes());
    }
    let mut charset = [0u8; CHARSET_BYTES];
    let length = settings.options.charset.len().min(CHARSET_BYTES - 1);
    charset[..length].copy_from_slice(&settings.options.charset.as_bytes()[..length]);
    header.extend_from_slice(&charset);
    Ok(header)
}

pub(super) fn compile(
    settings: &Settings,
    kind: DictionaryKind,
    inputs: &[PathBuf],
    output: &Path,
) -> Result<Vec<usize>, Error> {
    settings.require_supported()?;
    settings.require_factor()?;
    let mut resources = Resources::new(settings)?;
    let pos_ids = PosIds::open(&settings.dic_dir)?;

    let ReadEntries {
        mut entries,
        mut features,
        counts,
    } = read_entries(settings, kind, inputs, &mut resources, &pos_ids)?;
    if settings.options.wakati {
        features.push(0);
    }
    if entries.is_empty() {
        return Err(Error::build(output, "no entries to compile"));
    }

    entries.sort_by(|a, b| a.surface.cmp(&b.surface));
    let array = {
        let groups = group_by_surface(&entries, output)?;
        darts::build(&groups.keys, &groups.values)
    };
    let tokens = encode_tokens(&entries);
    let lengths = SectionLengths {
        entries: entries.len(),
        array: array.len(),
        tokens: tokens.len(),
        features: features.len(),
    };
    let header = encode_header(settings, kind, resources.sizes, &lengths, output)?;

    write_file(output, |file| {
        file.write_all(&header)?;
        file.write_all(&array)?;
        file.write_all(&tokens)?;
        file.write_all(&features)
    })?;
    Ok(counts)
}

pub(super) fn assign_costs(
    settings: &Settings,
    inputs: &[PathBuf],
    output: &Path,
) -> Result<Vec<usize>, Error> {
    settings.require_supported()?;
    settings.require_factor()?;
    let dir = &settings.dic_dir;
    let mut resources = Resources::new(settings)?;
    resources.rewriter = Some(Rewriter::open(&resources.rewrite_def())?);
    resources.cost = Some(settings.open_cost_model()?);
    resources.open_ids()?;

    let mut text = Vec::new();
    let mut counts = Vec::new();
    for input in inputs {
        let mut lines = Lines::open(input, Some(LINE_BUFFER_BYTES))?;
        let mut count = 0;
        while let Some(line) = lines.next_line()? {
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            let columns = tokenize_csv(line, CSV_COLUMNS);
            if columns.len() != CSV_COLUMNS {
                return Err(Error::build(input, format!("format error: {shown}")));
            }
            let mut word = cstr(line, columns[SURFACE_COLUMN]).to_vec();
            let feature = cstr(line, columns[FEATURE_COLUMN]).to_vec();
            let cost = resources.calc_cost(&word, &feature)?;
            let (left, right) = resources.ids_of(&feature)?;
            if !resources.ids_are_valid(left, right) {
                return Err(invalid_ids_error(left, right, dir));
            }
            escape_csv_element(&mut word);
            text.extend_from_slice(&word);
            text.extend_from_slice(format!(",{left},{right},{cost},").as_bytes());
            text.extend_from_slice(&feature);
            text.push(b'\n');
            count += 1;
        }
        counts.push(count);
    }
    write_file(output, |file| file.write_all(&text))?;
    Ok(counts)
}
