use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::read_file;
use crate::connection_templates::{ConnectionTemplates, TemplatePart};
use crate::tool_error::{io_error, rebuild_error, rebuild_error_at};
use mecab_rs::__private::files::{
    DICRC_FILE, FEATURE_DEF_FILE, LEFT_ID_DEF_FILE, MODEL_DEF_FILE, RIGHT_ID_DEF_FILE,
};
use mecab_rs::__private::matrix::HotShape;
use mecab_rs::__private::template_universe::{
    id_rows, parse_side, project, read_bigrams, side_keys,
};
use mecab_rs::Error;
use mecab_rs::IoAction;

pub(crate) const DEFAULT_HOT_LEFT: usize = 4480;
pub(crate) const DEFAULT_HOT_RIGHT: usize = 3136;
pub const DEFAULT_HOT_CELLS: usize = 14_000_000;

const MODEL_HEADER_LINES: usize = 7;
const NO_FIRE_LIMIT: usize = 0xFFFF;
const READ_BUFFER_BYTES: usize = 1 << 20;

struct Source {
    name: String,
    left_keys: Vec<Option<String>>,
    right_keys: Vec<Option<String>>,
    left_set: HashSet<String>,
    right_set: HashSet<String>,
    weights: HashMap<(Box<str>, u32), f64>,
}

fn read_cost_factor(dicrc: &Path) -> Result<u32, Error> {
    let text =
        String::from_utf8(read_file(dicrc)?).map_err(|_| rebuild_error(dicrc, "not UTF-8"))?;
    for line in text.lines() {
        if line.starts_with("cost-factor") {
            return line
                .split('=')
                .nth(1)
                .and_then(|value| value.trim().parse().ok())
                .ok_or_else(|| rebuild_error(dicrc, "cost-factor is not an integer"));
        }
    }
    Err(rebuild_error(dicrc, "no cost-factor"))
}

fn index_by_name(sources: &[Source]) -> HashMap<String, Vec<usize>> {
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, source) in sources.iter().enumerate() {
        by_name.entry(source.name.clone()).or_default().push(index);
    }
    by_name
}

fn read_model_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    model_def: &Path,
) -> Result<bool, Error> {
    line.clear();
    if reader
        .read_until(b'\n', line)
        .map_err(io_error(IoAction::Read, model_def))?
        == 0
    {
        return Ok(false);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(true)
}

struct WeightEntry<'line, 'names> {
    targets: &'names [usize],
    rest: &'line str,
    weight: f64,
}

fn parse_weight_line<'line, 'names>(
    line: &'line [u8],
    number: usize,
    model_def: &Path,
    by_name: &'names HashMap<String, Vec<usize>>,
) -> Result<Option<WeightEntry<'line, 'names>>, Error> {
    let malformed = || rebuild_error_at(model_def, number, "expected `weight<TAB>name:key`");
    let tab = line
        .iter()
        .position(|&byte| byte == b'\t')
        .ok_or_else(malformed)?;
    let key = &line[tab + 1..];
    let colon = key
        .iter()
        .position(|&byte| byte == b':')
        .ok_or_else(malformed)?;
    let Ok(name) = std::str::from_utf8(&key[..colon]) else {
        return Ok(None);
    };
    let Some(targets) = by_name.get(name) else {
        return Ok(None);
    };
    let rest = std::str::from_utf8(&key[colon + 1..])
        .map_err(|_| rebuild_error_at(model_def, number, "not UTF-8"))?;
    let weight: f64 = std::str::from_utf8(&line[..tab])
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| rebuild_error_at(model_def, number, "the weight is not a number"))?;
    Ok(Some(WeightEntry {
        targets,
        rest,
        weight,
    }))
}

fn record_weight(sources: &mut [Source], entry: &WeightEntry) {
    for (position, _) in entry.rest.match_indices('/') {
        let (left, right) = (&entry.rest[..position], &entry.rest[position + 1..]);
        for &target in entry.targets {
            let source = &mut sources[target];
            if source.left_set.contains(left) && source.right_set.contains(right) {
                source
                    .weights
                    .insert((Box::from(entry.rest), position as u32), entry.weight);
            }
        }
    }
}

fn read_weights(model_def: &Path, sources: &mut [Source]) -> Result<(), Error> {
    let by_name = index_by_name(sources);
    let file = File::open(model_def).map_err(io_error(IoAction::Open, model_def))?;
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
    let mut line = Vec::new();
    let mut number = 0usize;
    while read_model_line(&mut reader, &mut line, model_def)? {
        number += 1;
        if number <= MODEL_HEADER_LINES {
            continue;
        }
        if let Some(entry) = parse_weight_line(&line, number, model_def, &by_name)? {
            record_weight(sources, &entry);
        }
    }
    Ok(())
}

fn split((rest, position): &(Box<str>, u32)) -> (&str, &str) {
    let (left, right) = rest.split_at(*position as usize);
    (left, &right[1..])
}

struct RelevantIds<'a>(Vec<&'a str>);

impl<'a> RelevantIds<'a> {
    fn collect(source: &'a Source, side: fn(&'a (Box<str>, u32)) -> &'a str) -> Self {
        let keys: BTreeSet<&str> = source.weights.keys().map(side).collect();
        Self(keys.into_iter().collect())
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn as_slice(&self) -> &[&'a str] {
        &self.0
    }

    fn index_of(&self, key: &str) -> usize {
        let index = self.0.partition_point(|&candidate| candidate < key);
        debug_assert_eq!(self.0.get(index), Some(&key));
        index
    }
}

fn left_of(key: &(Box<str>, u32)) -> &str {
    split(key).0
}

fn right_of(key: &(Box<str>, u32)) -> &str {
    split(key).1
}

fn compile(source: &Source, path: &Path) -> Result<TemplatePart, Error> {
    let left_relevant = RelevantIds::collect(source, left_of);
    let right_relevant = RelevantIds::collect(source, right_of);
    if left_relevant.len() > NO_FIRE_LIMIT || right_relevant.len() > NO_FIRE_LIMIT {
        return Err(rebuild_error(
            path,
            format!("template {} has more than 65535 distinct keys", source.name),
        ));
    }
    let mut items: Vec<(u16, u16, f64)> = source
        .weights
        .iter()
        .map(|(key, &weight)| {
            let (left, right) = split(key);
            (
                right_relevant.index_of(right) as u16,
                left_relevant.index_of(left) as u16,
                weight,
            )
        })
        .collect();
    items.sort_unstable_by_key(|&(right, left, _)| (right, left));
    let mut indptr = vec![0u32; right_relevant.len() + 1];
    for &(right, _, _) in &items {
        indptr[usize::from(right) + 1] += 1;
    }
    for at in 1..indptr.len() {
        indptr[at] += indptr[at - 1];
    }
    Ok(TemplatePart {
        left_count: left_relevant.len(),
        right_count: right_relevant.len(),
        left_projection: project(&source.left_keys, left_relevant.as_slice()),
        right_projection: project(&source.right_keys, right_relevant.as_slice()),
        indptr,
        left_index: items.iter().map(|item| item.1).collect(),
        weight: items.iter().map(|item| item.2).collect(),
    })
}

pub fn compile_templates(
    source_dir: &Path,
    left_id_def: &str,
    right_id_def: &str,
) -> Result<ConnectionTemplates, Error> {
    let cost_factor = read_cost_factor(&source_dir.join(DICRC_FILE))?;
    let feature_def = source_dir.join(FEATURE_DEF_FILE);
    let left_rows = id_rows(right_id_def, &source_dir.join(RIGHT_ID_DEF_FILE))?;
    let right_rows = id_rows(left_id_def, &source_dir.join(LEFT_ID_DEF_FILE))?;
    let mut sources = Vec::new();
    for (name, body) in read_bigrams(&feature_def)? {
        let (left_body, right_body) = body
            .split_once('/')
            .ok_or_else(|| rebuild_error(&feature_def, format!("template {name} has no `/`")))?;
        let left_keys = side_keys(&parse_side(left_body, true), &left_rows);
        let right_keys = side_keys(&parse_side(right_body, false), &right_rows);
        let set = |keys: &[Option<String>]| -> HashSet<String> {
            keys.iter().flatten().cloned().collect()
        };
        sources.push(Source {
            name,
            left_set: set(&left_keys),
            right_set: set(&right_keys),
            left_keys,
            right_keys,
            weights: HashMap::new(),
        });
    }
    let model_def = source_dir.join(MODEL_DEF_FILE);
    read_weights(&model_def, &mut sources)?;
    let parts = sources
        .iter()
        .map(|source| compile(source, &model_def))
        .collect::<Result<Vec<_>, _>>()?;
    ConnectionTemplates::new(
        f64::from(cost_factor),
        left_rows.len(),
        right_rows.len(),
        parts,
    )
}

pub(super) fn hot_shape_or_default(
    hot: Option<HotShape>,
    left_size: usize,
    right_size: usize,
) -> HotShape {
    hot.unwrap_or_else(|| HotShape::rectangle(DEFAULT_HOT_LEFT, DEFAULT_HOT_RIGHT))
        .clamped(left_size, right_size)
}
