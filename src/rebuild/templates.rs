
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::thread;

use super::{bad, io_error, read_file};
use crate::build::write_file;
use crate::{ConnectionTemplates, Error, TemplatePart};

pub const DEFAULT_HOT_LEFT: usize = 7104;
pub const DEFAULT_HOT_RIGHT: usize = 4992;
pub const MAX_HOT_THREADS: usize = 8;

const MODEL_HEADER_LINES: usize = 7;
const NO_FIRE_LIMIT: usize = 0xFFFF;
const TEMPLATE_FILE: &str = "matrix.tpl";
const INT_TEMPLATE_FILE: &str = "matrix.itp";
const HOT_FILE: &str = "matrix.hot";

pub(super) fn remove_stale(dict_dir: &Path, name: &str) -> Result<(), Error> {
    let path = dict_dir.join(name);
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(io_error(&path)(error)),
        _ => Ok(()),
    }
}

pub fn default_threads() -> usize {
    thread::available_parallelism().map_or(1, |count| count.get().min(MAX_HOT_THREADS))
}

enum Piece {
    Text(String),
    Field { optional: bool, index: usize },
}

fn parse_side(body: &str, left: bool) -> Vec<Piece> {
    let bytes = body.as_bytes();
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && let Some((side_is_left, optional, index, end)) = field_at(bytes, at)
        {
            if !text.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut text)));
            }
            if side_is_left == left {
                pieces.push(Piece::Field { optional, index });
            }
            at = end;
        } else {
            let length = body[at..].chars().next().map_or(1, char::len_utf8);
            text.push_str(&body[at..at + length]);
            at += length;
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    pieces
}

fn field_at(bytes: &[u8], at: usize) -> Option<(bool, bool, usize, usize)> {
    let side = match *bytes.get(at + 1)? {
        b'L' => true,
        b'R' => false,
        _ => return None,
    };
    let mut cursor = at + 2;
    let optional = bytes.get(cursor) == Some(&b'?');
    if optional {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'[') {
        return None;
    }
    cursor += 1;
    let digits = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == digits || bytes.get(cursor) != Some(&b']') {
        return None;
    }
    let index = std::str::from_utf8(&bytes[digits..cursor])
        .ok()?
        .parse()
        .unwrap_or(usize::MAX);
    Some((side, optional, index, cursor + 1))
}

fn csv_fields(attributes: &str) -> Vec<String> {
    #[derive(PartialEq)]
    enum State {
        Start,
        Plain,
        Quoted,
        QuoteInQuoted,
    }
    if attributes.is_empty() {
        return Vec::new();
    }
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut state = State::Start;
    for character in attributes.chars() {
        state = match (state, character) {
            (State::Start | State::Plain | State::QuoteInQuoted, ',') => {
                fields.push(std::mem::take(&mut field));
                State::Start
            }
            (State::Start, '"') => State::Quoted,
            (State::Quoted, '"') => State::QuoteInQuoted,
            (State::QuoteInQuoted, '"') => {
                field.push('"');
                State::Quoted
            }
            (State::Quoted, other) => {
                field.push(other);
                State::Quoted
            }
            (_, other) => {
                field.push(other);
                State::Plain
            }
        };
    }
    fields.push(field);
    fields
}

fn id_rows(text: &str, path: &Path) -> Result<Vec<Vec<String>>, Error> {
    let mut rows = Vec::new();
    for line in text.split_terminator('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (index, attributes) = line
            .split_once(' ')
            .ok_or_else(|| bad(path, format!("row {} has no attributes", rows.len() + 1)))?;
        if index.parse::<usize>().ok() != Some(rows.len()) {
            return Err(bad(
                path,
                format!(
                    "row {} does not carry the id {}",
                    rows.len() + 1,
                    rows.len()
                ),
            ));
        }
        rows.push(csv_fields(attributes));
    }
    Ok(rows)
}

fn side_keys(pieces: &[Piece], rows: &[Vec<String>]) -> Vec<Option<String>> {
    rows.iter()
        .map(|row| {
            let mut key = String::new();
            for piece in pieces {
                match piece {
                    Piece::Text(text) => key.push_str(text),
                    Piece::Field { optional, index } => {
                        let value = row.get(*index)?;
                        if *optional && value == "*" {
                            return None;
                        }
                        key.push_str(value);
                    }
                }
            }
            Some(key)
        })
        .collect()
}

struct Source {
    name: String,
    left_keys: Vec<Option<String>>,
    right_keys: Vec<Option<String>>,
    left_set: HashSet<String>,
    right_set: HashSet<String>,
    weights: HashMap<(Box<str>, u32), f64>,
}

pub(super) fn read_bigrams(feature_def: &Path) -> Result<Vec<(String, String)>, Error> {
    let text =
        String::from_utf8(read_file(feature_def)?).map_err(|_| bad(feature_def, "not UTF-8"))?;
    let mut bigrams = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("BIGRAM ") {
            let (name, body) = rest
                .split_once(':')
                .ok_or_else(|| bad(feature_def, format!("BIGRAM line without `:`: {line}")))?;
            bigrams.push((name.to_string(), body.to_string()));
        }
    }
    Ok(bigrams)
}

fn read_cost_factor(dicrc: &Path) -> Result<u32, Error> {
    let text = String::from_utf8(read_file(dicrc)?).map_err(|_| bad(dicrc, "not UTF-8"))?;
    for line in text.lines() {
        if line.starts_with("cost-factor") {
            return line
                .split('=')
                .nth(1)
                .and_then(|value| value.trim().parse().ok())
                .ok_or_else(|| bad(dicrc, "cost-factor is not an integer"));
        }
    }
    Err(bad(dicrc, "no cost-factor"))
}

fn read_weights(model_def: &Path, sources: &mut [Source]) -> Result<(), Error> {
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, source) in sources.iter().enumerate() {
        by_name.entry(source.name.clone()).or_default().push(index);
    }
    let file = File::open(model_def).map_err(io_error(model_def))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::new();
    let mut number = 0usize;
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .map_err(io_error(model_def))?
            == 0
        {
            return Ok(());
        }
        number += 1;
        if number <= MODEL_HEADER_LINES {
            continue;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let malformed = || {
            bad(
                model_def,
                format!("line {number}: expected `weight<TAB>name:key`"),
            )
        };
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
            continue;
        };
        let Some(targets) = by_name.get(name) else {
            continue;
        };
        let rest = std::str::from_utf8(&key[colon + 1..])
            .map_err(|_| bad(model_def, format!("line {number}: not UTF-8")))?;
        let weight: f64 = std::str::from_utf8(&line[..tab])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| {
                bad(
                    model_def,
                    format!("line {number}: the weight is not a number"),
                )
            })?;
        for (position, _) in rest.match_indices('/') {
            let (left, right) = (&rest[..position], &rest[position + 1..]);
            for &target in targets {
                let source = &mut sources[target];
                if source.left_set.contains(left) && source.right_set.contains(right) {
                    source
                        .weights
                        .insert((Box::from(rest), position as u32), weight);
                }
            }
        }
    }
}

fn project(keys: &[Option<String>], relevant: &[&str]) -> Vec<u16> {
    keys.iter()
        .map(|key| {
            let index = key
                .as_deref()
                .and_then(|key| relevant.binary_search(&key).ok())
                .unwrap_or(relevant.len());
            index as u16
        })
        .collect()
}

fn split((rest, position): &(Box<str>, u32)) -> (&str, &str) {
    let (left, right) = rest.split_at(*position as usize);
    (left, &right[1..])
}

fn compile(source: &Source, path: &Path) -> Result<TemplatePart, Error> {
    let left_relevant: Vec<&str> = source
        .weights
        .keys()
        .map(|key| split(key).0)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let right_relevant: Vec<&str> = source
        .weights
        .keys()
        .map(|key| split(key).1)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if left_relevant.len() > NO_FIRE_LIMIT || right_relevant.len() > NO_FIRE_LIMIT {
        return Err(bad(
            path,
            format!("template {} has more than 65535 distinct keys", source.name),
        ));
    }
    let mut items: Vec<(u16, u16, f64)> = source
        .weights
        .iter()
        .map(|(key, &weight)| {
            let (left, right) = split(key);
            let left_index = left_relevant.binary_search(&left).expect("collected above");
            let right_index = right_relevant
                .binary_search(&right)
                .expect("collected above");
            (right_index as u16, left_index as u16, weight)
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
        left_projection: project(&source.left_keys, &left_relevant),
        right_projection: project(&source.right_keys, &right_relevant),
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
    let cost_factor = read_cost_factor(&source_dir.join("dicrc"))?;
    let feature_def = source_dir.join("feature.def");
    let left_rows = id_rows(right_id_def, &source_dir.join("right-id.def"))?;
    let right_rows = id_rows(left_id_def, &source_dir.join("left-id.def"))?;
    let mut sources = Vec::new();
    for (name, body) in read_bigrams(&feature_def)? {
        let (left_body, right_body) = body
            .split_once('/')
            .ok_or_else(|| bad(&feature_def, format!("template {name} has no `/`")))?;
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
    let model_def = source_dir.join("model.def");
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

pub(super) struct UniverseSide {
    pub(super) projection: Vec<u16>,
    pub(super) count: usize,
}

pub(super) fn universe_projections(
    bigrams: &[(String, String)],
    left_id_def: &str,
    right_id_def: &str,
    source: &Path,
) -> Result<Vec<(UniverseSide, UniverseSide)>, Error> {
    let left_rows = id_rows(right_id_def, &source.join("right-id.def"))?;
    let right_rows = id_rows(left_id_def, &source.join("left-id.def"))?;
    let side = |keys: Vec<Option<String>>| -> Result<UniverseSide, Error> {
        let distinct: BTreeSet<&str> = keys.iter().flatten().map(String::as_str).collect();
        if distinct.len() > NO_FIRE_LIMIT {
            return Err(bad(source, "a template has more than 65535 distinct keys"));
        }
        let distinct: Vec<&str> = distinct.into_iter().collect();
        Ok(UniverseSide {
            projection: project(&keys, &distinct),
            count: distinct.len(),
        })
    };
    bigrams
        .iter()
        .map(|(name, body)| {
            let (left_body, right_body) = body
                .split_once('/')
                .ok_or_else(|| bad(source, format!("template {name} has no `/`")))?;
            Ok((
                side(side_keys(&parse_side(left_body, true), &left_rows))?,
                side(side_keys(&parse_side(right_body, false), &right_rows))?,
            ))
        })
        .collect()
}

pub fn write_matrix_files(
    templates: &ConnectionTemplates,
    dict_dir: &Path,
    hot: Option<(usize, usize)>,
    threads: usize,
) -> Result<(), Error> {
    let (hot_left, hot_right) = hot.unwrap_or((DEFAULT_HOT_LEFT, DEFAULT_HOT_RIGHT));
    let (hot_left, hot_right) = (
        hot_left.min(templates.left_size()),
        hot_right.min(templates.right_size()),
    );
    remove_stale(dict_dir, INT_TEMPLATE_FILE)?;
    write_file(&dict_dir.join(TEMPLATE_FILE), |writer| {
        templates.write_templates(writer)
    })?;
    write_file(&dict_dir.join(HOT_FILE), |writer| {
        templates.write_hot_matrix(hot_left, hot_right, threads, writer)
    })
}
