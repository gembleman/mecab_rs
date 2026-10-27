mod lexicon;
mod matrix;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use lzma_rust2::{XzOptions, XzReader, XzWriter};

use super::model::Model;
use super::source::read_dictionary;
use crate::Error;
use crate::build::write_file;

const MAGIC: &[u8; 4] = b"MRDT";
const VERSION: u32 = 2;
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const NICE_LEN: u32 = 273;
const DEPTH_LIMIT: i32 = 512;
const MAX_DICT_SIZE: u32 = 512 << 20;

pub(super) use matrix::encode as matrix_part;

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |state, &byte| {
        (state ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

fn bad(path: &Path, reason: &'static str) -> Error {
    Error::Format {
        file: path.to_path_buf(),
        reason,
    }
}

fn io_error(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(super) struct Part {
    pub(super) name: &'static str,
    pub(super) encoding: u32,
    pub(super) bytes: Vec<u8>,
}

pub(super) struct Parts<'a> {
    path: &'a Path,
    parts: Vec<(&'a str, u32, &'a [u8])>,
}

impl<'a> Parts<'a> {
    fn get(&self, name: &str) -> Result<(u32, &'a [u8]), Error> {
        self.parts
            .iter()
            .find(|part| part.0 == name)
            .map(|&(_, encoding, bytes)| (encoding, bytes))
            .ok_or_else(|| bad(self.path, "a part is missing"))
    }

    fn plain(&self, name: &str, encoding: u32) -> Result<&'a [u8], Error> {
        match self.get(name)? {
            (found, bytes) if found == encoding => Ok(bytes),
            _ => Err(Error::Unsupported("a part with an unknown encoding")),
        }
    }

    fn corrupt(&self, reason: &'static str) -> Error {
        bad(self.path, reason)
    }
}

fn dicrc_part(model: &Model) -> Part {
    let unknown = model.unknown_feature.as_deref().unwrap_or_default();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(model.bos_feature.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(unknown.len() as u32).to_le_bytes());
    bytes.extend_from_slice(model.bos_feature.as_bytes());
    bytes.extend_from_slice(unknown.as_bytes());
    Part {
        name: "dicrc",
        encoding: 0,
        bytes,
    }
}

pub(super) fn encode_parts(model: &Model, matrix: Part) -> Result<Vec<Part>, Error> {
    let mut parts = lexicon::encode(model)?;
    parts.push(matrix);
    parts.push(Part {
        name: "char table",
        encoding: 0,
        bytes: model.char_table.clone(),
    });
    parts.push(dicrc_part(model));
    Ok(parts)
}

pub(super) fn container(parts: &[Part]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
    for part in parts {
        out.push(part.name.len() as u8);
        out.extend_from_slice(part.name.as_bytes());
        out.extend_from_slice(&part.encoding.to_le_bytes());
        out.extend_from_slice(&(part.bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(&part.bytes);
    }
    let digest = fnv1a64(&out);
    out.extend_from_slice(&digest.to_le_bytes());
    out
}

pub(super) fn parse_container<'a>(payload: &'a [u8], path: &'a Path) -> Result<Parts<'a>, Error> {
    let body_length = payload
        .len()
        .checked_sub(8)
        .ok_or_else(|| bad(path, "the data is too short"))?;
    let (body, digest) = payload.split_at(body_length);
    if fnv1a64(body) != u64::from_le_bytes(digest.try_into().expect("eight bytes")) {
        return Err(bad(path, "the FNV digest does not match"));
    }
    let mut at = 0usize;
    let mut take = |length: usize| -> Result<&'a [u8], Error> {
        let end = at
            .checked_add(length)
            .filter(|&end| end <= body.len())
            .ok_or_else(|| bad(path, "the data ends early"))?;
        let slice = &body[at..end];
        at = end;
        Ok(slice)
    };
    let word = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().expect("four bytes"));
    if take(4)? != MAGIC || word(take(4)?) != VERSION {
        return Err(bad(path, "the magic or the version is not valid"));
    }
    let count = word(take(4)?) as usize;
    let mut parts: Vec<(&str, u32, &[u8])> = Vec::new();
    for _ in 0..count {
        let name_length = usize::from(take(1)?[0]);
        let name = str::from_utf8(take(name_length)?)
            .map_err(|_| bad(path, "a part name is not valid UTF-8"))?;
        let encoding = word(take(4)?);
        let length = u64::from_le_bytes(take(8)?.try_into().expect("eight bytes"));
        let length = usize::try_from(length).map_err(|_| bad(path, "a part is too long"))?;
        if parts.iter().any(|part| part.0 == name) {
            return Err(bad(path, "a part is repeated"));
        }
        parts.push((name, encoding, take(length)?));
    }
    if at != body.len() {
        return Err(bad(path, "the data has trailing bytes"));
    }
    Ok(Parts { path, parts })
}

fn model_from_parts(parts: &Parts, next_to: &Path) -> Result<Model, Error> {
    let mut model = Model::default();
    lexicon::decode(parts, &mut model)?;
    (model.matrix, model.templates) = matrix::decode(parts, next_to)?;
    model.integer_templates = model.templates.starts_with(b"MXIT");
    model.char_table = parts.plain("char table", 0)?.to_vec();
    let dicrc = parts.plain("dicrc", 0)?;
    let corrupt = |reason| parts.corrupt(reason);
    if dicrc.len() < 8 {
        return Err(corrupt("dicrc is too short"));
    }
    let bos_length = u32::from_le_bytes(dicrc[..4].try_into().expect("four bytes")) as usize;
    let unknown_length = u32::from_le_bytes(dicrc[4..8].try_into().expect("four bytes")) as usize;
    if bos_length.checked_add(unknown_length).map(|sum| sum + 8) != Some(dicrc.len()) {
        return Err(corrupt("the lengths in dicrc do not match"));
    }
    let text = |bytes: &[u8]| {
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("dicrc is not valid UTF-8"))
    };
    model.bos_feature = text(&dicrc[8..8 + bos_length])?;
    model.unknown_feature = (unknown_length > 0)
        .then(|| text(&dicrc[8 + bos_length..]))
        .transpose()?;
    Ok(model)
}

pub(super) fn compress(payload: &[u8], path: &Path) -> Result<Vec<u8>, Error> {
    let mut options = XzOptions::with_preset(9);
    options.lzma_options.nice_len = NICE_LEN;
    options.lzma_options.depth_limit = DEPTH_LIMIT;
    options.lzma_options.dict_size = options
        .lzma_options
        .dict_size
        .max(u32::try_from(payload.len()).unwrap_or(u32::MAX))
        .min(MAX_DICT_SIZE);
    let failed = |error: std::io::Error| Error::Rebuild {
        path: path.to_path_buf(),
        reason: format!("xz: {error}"),
    };
    let mut writer = XzWriter::new(Vec::new(), options).map_err(failed)?;
    writer.write_all(payload).map_err(failed)?;
    writer.finish().map_err(failed)
}

fn work_directory(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.matrix"))
}

pub fn write_transport(dict_dir: &Path, out_xz: &Path) -> Result<u64, Error> {
    let model = read_dictionary(dict_dir)?;
    let matrix = matrix_part(dict_dir, &model, out_xz)?;
    let payload = container(&encode_parts(&model, matrix)?);
    drop(model);
    let compressed = compress(&payload, out_xz)?;
    write_file(out_xz, |writer| writer.write_all(&compressed))?;
    Ok(compressed.len() as u64)
}

pub fn expand(xz_file: &Path, out_mrd: &Path) -> Result<(), Error> {
    let file = std::fs::File::open(xz_file).map_err(io_error(xz_file))?;
    let mut payload = Vec::new();
    XzReader::new(std::io::BufReader::new(file), false)
        .read_to_end(&mut payload)
        .map_err(|error| Error::Rebuild {
            path: xz_file.to_path_buf(),
            reason: format!("xz: {error}"),
        })?;
    let model = model_from_parts(&parse_container(&payload, xz_file)?, out_mrd)?;
    drop(payload);
    let runtime = model.encode_runtime()?;
    write_file(out_mrd, |writer| runtime.write_to(writer))
}
