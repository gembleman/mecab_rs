
use std::path::Path;

use super::{Part, Parts, bad, io_error, work_directory};
use crate::Error;
use crate::packed::model::Model;

const ENCODING_DENSE_DELTA: u32 = 1;
const ENCODING_TEMPLATES_XZ: u32 = 2;

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

fn regenerate(xz: &[u8], hot: (usize, usize), next_to: &Path) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let directory = work_directory(next_to);
    std::fs::create_dir_all(&directory).map_err(io_error(&directory))?;
    let run = || {
        let pack = directory.join("matrix.xz");
        std::fs::write(&pack, xz).map_err(io_error(&pack))?;
        crate::rebuild::xz::unpack(
            &pack,
            &directory,
            Some(hot),
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

pub(in crate::packed) fn encode(dict_dir: &Path, model: &Model, out: &Path) -> Result<Part, Error> {
    if model.templates.is_empty() {
        return Ok(Part {
            name: "matrix",
            encoding: ENCODING_DENSE_DELTA,
            bytes: row_delta(&model.matrix, true),
        });
    }
    let hot = (
        usize::from(u16::from_le_bytes([model.matrix[4], model.matrix[5]])),
        usize::from(u16::from_le_bytes([model.matrix[6], model.matrix[7]])),
    );
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
    let (regenerated_hot, regenerated_templates) = regenerate(&xz, hot, out)?;
    if regenerated_hot != model.matrix || regenerated_templates != model.templates {
        return Err(Error::Unsupported(
            "matrix.hot and matrix.tpl that matrix.xz does not regenerate",
        ));
    }
    let mut bytes = Vec::with_capacity(xz.len() + 8);
    bytes.extend_from_slice(&(hot.0 as u32).to_le_bytes());
    bytes.extend_from_slice(&(hot.1 as u32).to_le_bytes());
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
            let (Some(hot_left), Some(hot_right)) = (bytes.get(..4), bytes.get(4..8)) else {
                return Err(corrupt("the matrix is too short"));
            };
            let word = |word: &[u8]| u32::from_le_bytes(word.try_into().expect("four bytes"));
            let hot = (word(hot_left) as usize, word(hot_right) as usize);
            regenerate(&bytes[8..], hot, next_to)
        }
        _ => Err(Error::Unsupported("a matrix with an unknown encoding")),
    }
}
