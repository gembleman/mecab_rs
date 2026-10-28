use std::path::Path;

use super::{Part, Parts, bad, io_error, work_directory};
use crate::int_runtime::{is_runtime_form, runtime_form_of};
use crate::int_templates::DEFAULT_CLUSTER_COUNT;
use crate::packed::model::Model;
use crate::rebuild::default_threads;
use crate::rebuild::int_matrix::{IntContext, decode_payload, encode_payload};
use crate::{Error, HotShape, IntTemplates};

const ENCODING_DENSE_DELTA: u32 = 1;
const ENCODING_TEMPLATES_XZ: u32 = 2;
const ENCODING_INTEGER_MRIT: u32 = 4;
const ROW_LENGTH_BYTES: usize = 2;
const WORD_BYTES: usize = 4;
const ID_DEFINITIONS: [&str; 2] = ["left-id.def", "right-id.def"];
const FEATURE_DEFINITION: &str = "feature.def";
const ITP_FILE: &str = "matrix.itp";

fn row_delta(matrix: &[u8], encode: bool) -> Vec<u8> {
    let mut out = matrix.to_vec();
    let (left, right) = (
        usize::from(u16::from_le_bytes([matrix[0], matrix[1]])),
        usize::from(u16::from_le_bytes([matrix[2], matrix[3]])),
    );
    for row in out[4..].chunks_exact_mut(left.max(1) * 2).take(right) {
        let mut previous = 0u16;
        for cell in row.as_chunks_mut::<2>().0 {
            let value = u16::from_le_bytes([cell[0], cell[1]]);
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

fn regenerate(xz: &[u8], hot: &HotShape, next_to: &Path) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let directory = work_directory(next_to);
    std::fs::create_dir_all(&directory).map_err(io_error(&directory))?;
    let run = || {
        let pack = directory.join("matrix.xz");
        std::fs::write(&pack, xz).map_err(io_error(&pack))?;
        crate::rebuild::xz::unpack(
            &pack,
            &directory,
            Some(hot.clone()),
            crate::rebuild::default_threads(),
        )?;
        let hot_path = directory.join("matrix.hot");
        let templates_path = directory.join("matrix.tpl");
        Ok((
            std::fs::read(&hot_path).map_err(io_error(&hot_path))?,
            std::fs::read(&templates_path).map_err(io_error(&templates_path))?,
        ))
    };
    let result = run();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

fn read_templates(bytes: &[u8], path: &Path) -> Result<crate::ConnectionTemplates, Error> {
    let broken = || bad(path, "the templates are not valid");
    let mut at = 0;
    let mut take = |length: usize| -> Result<&[u8], Error> {
        let slice = bytes.get(at..at + length).ok_or_else(broken)?;
        at += length;
        Ok(slice)
    };
    let word = |slice: &[u8]| u32::from_le_bytes(slice.try_into().expect("four bytes")) as usize;
    if take(8)?[..4] != *b"MXTP" {
        return Err(broken());
    }
    let cost_factor = f64::from_le_bytes(take(8)?.try_into().expect("eight bytes"));
    let (left_size, right_size, part_count) = (word(take(4)?), word(take(4)?), word(take(4)?));
    take(4)?;
    let mut parts = Vec::new();
    for _ in 0..part_count {
        let (left_count, right_count, nnz) = (word(take(4)?), word(take(4)?), word(take(4)?));
        take(4)?;
        let mut padded = |count: usize, width: usize| -> Result<&[u8], Error> {
            let length = count.checked_mul(width).ok_or_else(broken)?;
            let items = take(length)?;
            take(length.next_multiple_of(8) - length)?;
            Ok(items)
        };
        let u16s = |items: &[u8]| -> Vec<u16> {
            items
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&item| u16::from_le_bytes(item))
                .collect()
        };
        let left_projection = u16s(padded(left_size, 2)?);
        let right_projection = u16s(padded(right_size, 2)?);
        let indptr = padded(right_count + 1, 4)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&item| u32::from_le_bytes(item))
            .collect();
        let left_index = u16s(padded(nnz, 2)?);
        let weight = padded(nnz, 8)?
            .as_chunks::<8>()
            .0
            .iter()
            .map(|&item| f64::from_le_bytes(item))
            .collect();
        parts.push(crate::TemplatePart {
            left_count,
            right_count,
            left_projection,
            right_projection,
            indptr,
            left_index,
            weight,
        });
    }
    crate::ConnectionTemplates::new(cost_factor, left_size, right_size, parts)
}

fn read_text(dict_dir: &Path, name: &str) -> Result<String, Error> {
    let path = dict_dir.join(name);
    std::fs::read_to_string(&path).map_err(io_error(&path))
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
    let mut bytes = Vec::new();
    templates
        .write_hot_matrix(hot, default_threads(), &mut bytes)
        .map_err(io_error(path))?;
    Ok(bytes)
}

fn full_templates(templates: &IntTemplates, path: &Path) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    templates
        .write_clustered_templates(DEFAULT_CLUSTER_COUNT, default_threads(), &mut bytes)
        .map_err(io_error(path))?;
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
            .ok_or_else(|| bad(self.path, "the integer matrix part ends early"))?;
        self.rest = tail;
        Ok(head)
    }

    fn word(&mut self) -> Result<usize, Error> {
        let bytes = self.bytes(WORD_BYTES)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")) as usize)
    }

    fn text(&mut self, length: usize) -> Result<&'a str, Error> {
        str::from_utf8(self.bytes(length)?)
            .map_err(|_| bad(self.path, "an id definition is not valid UTF-8"))
    }
}

fn parse_integer(bytes: &[u8], path: &Path) -> Result<(IntTemplates, HotShape), Error> {
    let mut take = Take { rest: bytes, path };
    let rows = take.word()?;
    let lengths_bytes = rows
        .checked_mul(ROW_LENGTH_BYTES)
        .ok_or_else(|| bad(path, "the hot rows are too many"))?;
    let hot = HotShape::from_lengths(
        take.bytes(lengths_bytes)?
            .as_chunks::<ROW_LENGTH_BYTES>()
            .0
            .iter()
            .map(|&length| u32::from(u16::from_le_bytes(length)))
            .collect(),
    );
    let (left_length, right_length) = (take.word()?, take.word()?);
    let left = take.text(left_length)?;
    let right = take.text(right_length)?;
    let templates = decode_payload(take.rest, left, right, path)?;
    if !hot.fits(templates.left_size(), templates.right_size()) {
        return Err(bad(path, "the hot block lies outside of the matrix"));
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
    let itp_path = dict_dir.join(ITP_FILE);
    if is_runtime_form(&model.templates) {
        return Err(Error::Unsupported(
            "matrix.itp in the runtime form; the transport form needs the full template file",
        ));
    }
    let (_, _, hot) = HotShape::parse(&model.matrix)
        .map_err(|reason| bad(&dict_dir.join("matrix.hot"), reason))?;
    let feature_definition = dict_dir.join(FEATURE_DEFINITION);
    if !feature_definition.is_file() {
        return Err(bad(
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

pub(in crate::packed) fn encode(dict_dir: &Path, model: &Model, out: &Path) -> Result<Part, Error> {
    if model.templates.is_empty() {
        return Ok(Part {
            name: "matrix",
            encoding: ENCODING_DENSE_DELTA,
            bytes: row_delta(&model.matrix, true),
        });
    }
    if model.integer_templates {
        return integer_part(dict_dir, model);
    }
    let (_, _, hot) = HotShape::parse(&model.matrix)
        .map_err(|reason| bad(&dict_dir.join("matrix.hot"), reason))?;
    let existing = dict_dir.join("matrix.xz");
    let xz = if existing.is_file() {
        std::fs::read(&existing).map_err(io_error(&existing))?
    } else {
        let templates = read_templates(&model.templates, &dict_dir.join("matrix.tpl"))?;
        let pack = work_directory(out).with_extension("pack");
        crate::rebuild::xz::pack(&templates, &pack)?;
        let bytes = std::fs::read(&pack).map_err(io_error(&pack));
        let _ = std::fs::remove_file(&pack);
        bytes?
    };
    let (regenerated_hot, regenerated_templates) = regenerate(&xz, &hot, out)?;
    if regenerated_hot != model.matrix || regenerated_templates != model.templates {
        return Err(Error::Unsupported(
            "matrix.hot and matrix.tpl that matrix.xz does not regenerate",
        ));
    }
    let mut bytes = Vec::with_capacity(xz.len() + 4 + hot.rows() * 2);
    bytes.extend_from_slice(&(hot.rows() as u32).to_le_bytes());
    for &length in hot.lengths() {
        bytes.extend_from_slice(&(length as u16).to_le_bytes());
    }
    bytes.extend_from_slice(&xz);
    Ok(Part {
        name: "matrix",
        encoding: ENCODING_TEMPLATES_XZ,
        bytes,
    })
}

pub(in crate::packed) fn decode(
    parts: &Parts,
    next_to: &Path,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let corrupt = |reason| parts.corrupt(reason);
    match parts.get("matrix")? {
        (ENCODING_DENSE_DELTA, bytes) => {
            let size = |at: usize| {
                bytes
                    .get(at..at + 2)
                    .map(|pair| usize::from(u16::from_le_bytes([pair[0], pair[1]])))
            };
            let (Some(left), Some(right)) = (size(0), size(2)) else {
                return Err(corrupt("the matrix is too short"));
            };
            if left == 0 || right == 0 || bytes.len() != 4 + left * right * 2 {
                return Err(corrupt("the matrix is not valid"));
            }
            Ok((row_delta(bytes, false), Vec::new()))
        }
        (ENCODING_TEMPLATES_XZ, bytes) => {
            let Some(rows) = bytes.get(..4) else {
                return Err(corrupt("the matrix is too short"));
            };
            let rows = u32::from_le_bytes(rows.try_into().expect("four bytes")) as usize;
            let lengths_end = rows
                .checked_mul(2)
                .and_then(|length| length.checked_add(4))
                .ok_or_else(|| corrupt("the hot rows are too many"))?;
            let lengths = bytes
                .get(4..lengths_end)
                .ok_or_else(|| corrupt("the matrix is too short"))?;
            let hot = HotShape::from_lengths(
                lengths
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&length| u32::from(u16::from_le_bytes(length)))
                    .collect(),
            );
            regenerate(&bytes[lengths_end..], &hot, next_to)
        }
        (ENCODING_INTEGER_MRIT, bytes) => decode_integer(bytes, parts.path),
        _ => Err(Error::Unsupported("a matrix with an unknown encoding")),
    }
}
