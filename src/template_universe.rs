#[cfg(feature = "xz")]
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::Error;
#[cfg(feature = "xz")]
use crate::dict_files::{LEFT_ID_DEF_FILE, RIGHT_ID_DEF_FILE};
use crate::error::io_error;

#[cfg(feature = "xz")]
const NO_FIRE_LIMIT: usize = 0xFFFF;

pub enum Piece {
    Text(String),
    Field { optional: bool, index: usize },
}

pub fn parse_side(body: &str, left: bool) -> Vec<Piece> {
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

pub fn id_rows(text: &str, path: &Path) -> Result<Vec<Vec<String>>, Error> {
    let mut rows = Vec::new();
    for line in text.split_terminator('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (index, attributes) = line.split_once(' ').ok_or_else(|| {
            Error::rebuild(path, format!("row {} has no attributes", rows.len() + 1))
        })?;
        if index.parse::<usize>().ok() != Some(rows.len()) {
            return Err(Error::rebuild(
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

pub fn side_keys(pieces: &[Piece], rows: &[Vec<String>]) -> Vec<Option<String>> {
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

pub fn read_bigrams(feature_def: &Path) -> Result<Vec<(String, String)>, Error> {
    let text = String::from_utf8(fs::read(feature_def).map_err(io_error(feature_def))?)
        .map_err(|_| Error::rebuild(feature_def, "not UTF-8"))?;
    let mut bigrams = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("BIGRAM ") {
            let (name, body) = rest.split_once(':').ok_or_else(|| {
                Error::rebuild(feature_def, format!("BIGRAM line without `:`: {line}"))
            })?;
            bigrams.push((name.to_string(), body.to_string()));
        }
    }
    Ok(bigrams)
}

pub fn project(keys: &[Option<String>], relevant: &[&str]) -> Vec<u16> {
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

#[cfg(feature = "xz")]
pub(crate) struct UniverseSide {
    pub projection: Vec<u16>,
    pub count: usize,
}

#[cfg(feature = "xz")]
pub(crate) fn universe_projections(
    bigrams: &[(String, String)],
    left_id_def: &str,
    right_id_def: &str,
    source: &Path,
) -> Result<Vec<(UniverseSide, UniverseSide)>, Error> {
    let left_rows = id_rows(right_id_def, &source.join(RIGHT_ID_DEF_FILE))?;
    let right_rows = id_rows(left_id_def, &source.join(LEFT_ID_DEF_FILE))?;
    let side = |keys: Vec<Option<String>>| -> Result<UniverseSide, Error> {
        let distinct: BTreeSet<&str> = keys.iter().flatten().map(String::as_str).collect();
        if distinct.len() > NO_FIRE_LIMIT {
            return Err(Error::rebuild(
                source,
                "a template has more than 65535 distinct keys",
            ));
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
                .ok_or_else(|| Error::rebuild(source, format!("template {name} has no `/`")))?;
            Ok((
                side(side_keys(&parse_side(left_body, true), &left_rows))?,
                side(side_keys(&parse_side(right_body, false), &right_rows))?,
            ))
        })
        .collect()
}
