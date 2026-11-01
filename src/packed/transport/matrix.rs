use std::path::Path;

use super::int_stream::{IntContext, decode_payload, encode_payload};
use super::{IoAction, MAX_PAYLOAD, Part, Parts, io_error};
use crate::Error;
use crate::dict_files::{
    FEATURE_DEF_FILE, LEFT_ID_DEF_FILE, MATRIX_HOT_FILE, MATRIX_ITP_FILE, RIGHT_ID_DEF_FILE,
};
use crate::hot_shape::{HotShape, MAX_KEYS};
use crate::int_runtime::{is_runtime_form, runtime_form_of};
use crate::int_templates::DEFAULT_CLUSTER_COUNT;
use crate::int_templates::IntTemplates;
use crate::packed::model::Model;
use crate::threads::default_threads;
use crate::wire::{u16_at, u32_at};

const ENCODING_DENSE_DELTA: u32 = 1;
const ENCODING_INTEGER_MRIT: u32 = 4;
const ROW_LENGTH_BYTES: usize = 2;
const WORD_BYTES: usize = 4;
const ID_DEFINITIONS: [&str; 2] = [LEFT_ID_DEF_FILE, RIGHT_ID_DEF_FILE];

fn row_delta(matrix: &[u8], encode: bool) -> Vec<u8> {
    let mut out = matrix.to_vec();
    let (left, right) = (
        usize::from(u16_at(matrix, 0)),
        usize::from(u16_at(matrix, 2)),
    );
    for row in out[4..].chunks_exact_mut(left.max(1) * 2).take(right) {
        let mut previous = 0u16;
        for cell in row.as_chunks_mut::<2>().0 {
            let value = u16_at(cell, 0);
            let stored = if encode {
                value.wrapping_sub(previous)
            } else {
                value.wrapping_add(previous)
            };
            previous = if encode { value } else { stored };
            cell.copy_from_slice(&stored.to_le_bytes());
        }
    }
    out
}

fn read_text(dict_dir: &Path, name: &str) -> Result<String, Error> {
    let path = dict_dir.join(name);
    std::fs::read_to_string(&path).map_err(io_error(IoAction::Read, &path))
}

fn length_word(length: usize) -> Result<[u8; WORD_BYTES], Error> {
    u32::try_from(length)
        .map(u32::to_le_bytes)
        .map_err(|_| Error::Unsupported("an integer matrix part with a field of 4 GiB or more"))
}

fn integer_bytes(hot: &HotShape, definitions: [&str; 2], payload: &[u8]) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&length_word(hot.rows())?);
    for &length in hot.lengths() {
        let length = u16::try_from(length)
            .map_err(|_| Error::Unsupported("a hot row longer than 65535 cells"))?;
        bytes.extend_from_slice(&length.to_le_bytes());
    }
    for definition in definitions {
        bytes.extend_from_slice(&length_word(definition.len())?);
    }
    for definition in definitions {
        bytes.extend_from_slice(definition.as_bytes());
    }
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

fn hot_matrix(templates: &IntTemplates, hot: &HotShape, path: &Path) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::with_capacity(hot.matrix_bytes());
    templates
        .write_hot_matrix(hot, default_threads(), &mut bytes)
        .map_err(io_error(IoAction::Write, path))?;
    Ok(bytes)
}

fn full_templates(templates: &IntTemplates, path: &Path) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    templates
        .write_clustered_templates(DEFAULT_CLUSTER_COUNT, default_threads(), &mut bytes)
        .map_err(io_error(IoAction::Write, path))?;
    Ok(bytes)
}

fn runtime_templates(templates: &IntTemplates, hot: &HotShape) -> Result<Vec<u8>, Error> {
    templates.runtime_form(hot, DEFAULT_CLUSTER_COUNT, default_threads())
}

struct Take<'a> {
    rest: &'a [u8],
    path: &'a Path,
}

impl<'a> Take<'a> {
    fn bytes(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let (head, tail) = self
            .rest
            .split_at_checked(length)
            .ok_or_else(|| Error::format(self.path, "the integer matrix part ends early"))?;
        self.rest = tail;
        Ok(head)
    }

    fn word(&mut self) -> Result<usize, Error> {
        let bytes = self.bytes(WORD_BYTES)?;
        Ok(u32_at(bytes, 0) as usize)
    }

    fn text(&mut self, length: usize) -> Result<&'a str, Error> {
        str::from_utf8(self.bytes(length)?)
            .map_err(|_| Error::format(self.path, "an id definition is not valid UTF-8"))
    }
}

fn parse_integer(bytes: &[u8], path: &Path) -> Result<(IntTemplates, HotShape), Error> {
    let mut take = Take { rest: bytes, path };
    let rows = take.word()?;
    let lengths_bytes = rows
        .checked_mul(ROW_LENGTH_BYTES)
        .ok_or_else(|| Error::format(path, "the hot rows are too many"))?;
    let hot = HotShape::from_lengths(
        take.bytes(lengths_bytes)?
            .as_chunks::<ROW_LENGTH_BYTES>()
            .0
            .iter()
            .map(|length| u32::from(u16_at(length, 0)))
            .collect(),
    );
    if hot.rows() > MAX_KEYS || hot.matrix_bytes() as u64 > MAX_PAYLOAD {
        return Err(Error::format(
            path,
            "the hot block is larger than any dictionary",
        ));
    }
    let (left_length, right_length) = (take.word()?, take.word()?);
    let left = take.text(left_length)?;
    let right = take.text(right_length)?;
    let templates = decode_payload(take.rest, left, right, path)?;
    if !hot.fits(templates.left_size(), templates.right_size()) {
        return Err(Error::format(
            path,
            "the hot block lies outside of the matrix",
        ));
    }
    Ok((templates, hot))
}

fn decode_integer(bytes: &[u8], path: &Path) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let (templates, hot) = parse_integer(bytes, path)?;
    std::thread::scope(|scope| {
        let matrix = scope.spawn(|| hot_matrix(&templates, &hot, path));
        let runtime = runtime_templates(&templates, &hot);
        let matrix = matrix.join().expect("the hot matrix writer panicked");
        Ok((matrix?, runtime?))
    })
}

fn check_regenerated(bytes: &[u8], model: &Model, path: &Path) -> Result<(), Error> {
    let (templates, hot) = parse_integer(bytes, path)?;
    let exact = hot_matrix(&templates, &hot, path)? == model.matrix
        && full_templates(&templates, path)? == model.templates;
    let runtime = runtime_templates(&templates, &hot)? == runtime_form_of(&model.templates, &hot)?;
    if exact && runtime {
        Ok(())
    } else {
        Err(Error::Unsupported(
            "matrix.hot and matrix.itp that the compact integer form does not regenerate",
        ))
    }
}

fn integer_part(dict_dir: &Path, model: &Model) -> Result<Part, Error> {
    let itp_path = dict_dir.join(MATRIX_ITP_FILE);
    if is_runtime_form(&model.templates) {
        return Err(Error::Unsupported(
            "matrix.itp in the runtime form; the transport form needs the full template file",
        ));
    }
    let (_, _, hot) = HotShape::parse(&model.matrix)
        .map_err(|reason| Error::format(&dict_dir.join(MATRIX_HOT_FILE), reason))?;
    let feature_definition = dict_dir.join(FEATURE_DEF_FILE);
    if !feature_definition.is_file() {
        return Err(Error::format(
            &feature_definition,
            "feature.def is missing; rebuild the runtime directory, which keeps feature.def",
        ));
    }
    let [left, right] = ID_DEFINITIONS.map(|name| read_text(dict_dir, name));
    let (left, right) = (left?, right?);
    let templates = IntTemplates::from_file_bytes(&model.templates, &itp_path)?;
    let context = IntContext::load(dict_dir, &left, &right)?;
    let payload = encode_payload(&templates, &context, &itp_path)?;
    let bytes = integer_bytes(&hot, [&left, &right], &payload)?;
    check_regenerated(&bytes, model, &itp_path)?;
    Ok(Part {
        name: "matrix",
        encoding: ENCODING_INTEGER_MRIT,
        bytes,
    })
}

pub fn encode(dict_dir: &Path, model: &Model) -> Result<Part, Error> {
    if model.templates.is_empty() {
        return Ok(Part {
            name: "matrix",
            encoding: ENCODING_DENSE_DELTA,
            bytes: row_delta(&model.matrix, true),
        });
    }
    integer_part(dict_dir, model)
}

pub(in crate::packed) fn decode(parts: &Parts) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let corrupt = |reason| parts.corrupt(reason);
    match parts.get("matrix")? {
        (ENCODING_DENSE_DELTA, bytes) => {
            let size = |at: usize| {
                bytes
                    .get(at..at + 2)
                    .map(|pair| usize::from(u16_at(pair, 0)))
            };
            let (Some(left), Some(right)) = (size(0), size(2)) else {
                return Err(corrupt("the matrix is too short"));
            };
            if left == 0 || right == 0 || bytes.len() != 4 + left * right * 2 {
                return Err(corrupt("the matrix is not valid"));
            }
            Ok((row_delta(bytes, false), Vec::new()))
        }
        (ENCODING_INTEGER_MRIT, bytes) => decode_integer(bytes, parts.path),
        (found, _) => {
            Err(parts.corrupt(format!("the part matrix has the unknown encoding {found}")))
        }
    }
}
