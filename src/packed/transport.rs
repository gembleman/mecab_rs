pub(crate) mod int_stream;
mod lexicon;
pub(crate) mod matrix;

use std::io::Read;
use std::path::Path;

use lzma_rust2::XzReader;

use super::model::{LexiconSections, Model};
use super::source::read_dictionary;
use crate::Error;
use crate::error::io_error;
use crate::file_write::write_file;
use crate::wire::{fnv1a64, u32_at, u64_at};

const MAGIC: &[u8; 4] = b"MRDT";
const VERSION: u32 = 3;

pub use matrix::encode as matrix_part;

pub struct Part {
    pub name: &'static str,
    pub encoding: u32,
    pub bytes: Vec<u8>,
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
            .ok_or_else(|| Error::format(self.path, "a part is missing"))
    }

    fn plain(&self, name: &str, encoding: u32) -> Result<&'a [u8], Error> {
        match self.get(name)? {
            (found, bytes) if found == encoding => Ok(bytes),
            _ => Err(Error::Unsupported("a part with an unknown encoding")),
        }
    }

    fn corrupt(&self, reason: &'static str) -> Error {
        Error::format(self.path, reason)
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

pub fn encode_parts(model: &Model, matrix: Part) -> Result<Vec<Part>, Error> {
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

pub fn container(parts: &[Part]) -> Vec<u8> {
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
        .ok_or_else(|| Error::format(path, "the data is too short"))?;
    let (body, digest) = payload.split_at(body_length);
    if fnv1a64(body) != u64_at(digest, 0) {
        return Err(Error::format(path, "the FNV digest does not match"));
    }
    let mut at = 0usize;
    let mut take = |length: usize| -> Result<&'a [u8], Error> {
        let end = at
            .checked_add(length)
            .filter(|&end| end <= body.len())
            .ok_or_else(|| Error::format(path, "the data ends early"))?;
        let slice = &body[at..end];
        at = end;
        Ok(slice)
    };
    let word = |bytes: &[u8]| u32_at(bytes, 0);
    if take(4)? != MAGIC {
        return Err(Error::format(path, "the magic is not valid"));
    }
    if word(take(4)?) != VERSION {
        return Err(Error::format(
            path,
            "the transport format version is not supported; pack the dictionary again",
        ));
    }
    let count = word(take(4)?) as usize;
    let mut parts: Vec<(&str, u32, &[u8])> = Vec::new();
    for _ in 0..count {
        let name_length = usize::from(take(1)?[0]);
        let name = str::from_utf8(take(name_length)?)
            .map_err(|_| Error::format(path, "a part name is not valid UTF-8"))?;
        let encoding = word(take(4)?);
        let length = u64_at(take(8)?, 0);
        let length =
            usize::try_from(length).map_err(|_| Error::format(path, "a part is too long"))?;
        if parts.iter().any(|part| part.0 == name) {
            return Err(Error::format(path, "a part is repeated"));
        }
        parts.push((name, encoding, take(length)?));
    }
    if at != body.len() {
        return Err(Error::format(path, "the data has trailing bytes"));
    }
    Ok(Parts { path, parts })
}

fn lexicon_model(parts: &Parts) -> Result<Model, Error> {
    let mut model = lexicon::decode(parts)?;
    model.char_table = parts.plain("char table", 0)?.to_vec();
    let dicrc = parts.plain("dicrc", 0)?;
    let corrupt = |reason| parts.corrupt(reason);
    if dicrc.len() < 8 {
        return Err(corrupt("dicrc is too short"));
    }
    let bos_length = u32_at(dicrc, 0) as usize;
    let unknown_length = u32_at(dicrc, 4) as usize;
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

fn attach_matrix(model: &mut Model, (matrix, templates): (Vec<u8>, Vec<u8>)) {
    model.matrix = matrix;
    model.templates = templates;
}

fn runtime_model(parts: &Parts) -> Result<(Model, LexiconSections), Error> {
    std::thread::scope(|scope| {
        let matrix = scope.spawn(|| matrix::decode(parts));
        let lexicon = lexicon_model(parts).and_then(|model| {
            let sections = model.encode_lexicon_sections()?;
            Ok((model, sections))
        });
        let matrix = matrix.join().expect("the matrix decoder panicked");
        let ((mut model, sections), matrix) = (lexicon?, matrix?);
        attach_matrix(&mut model, matrix);
        Ok((model, sections))
    })
}

pub fn transport_payload(dict_dir: &Path) -> Result<Vec<u8>, Error> {
    let model = read_dictionary(dict_dir)?;
    let matrix = matrix_part(dict_dir, &model)?;
    Ok(container(&encode_parts(&model, matrix)?))
}

pub fn expand(xz_file: &Path, out_mrd: &Path) -> Result<(), Error> {
    let file = std::fs::File::open(xz_file).map_err(io_error(xz_file))?;
    let mut payload = Vec::new();
    XzReader::new(std::io::BufReader::new(file), false)
        .read_to_end(&mut payload)
        .map_err(|error| Error::rebuild(xz_file, format!("xz: {error}")))?;
    let (model, sections) = runtime_model(&parse_container(&payload, xz_file)?)?;
    drop(payload);
    let runtime = model.finish_runtime(sections)?;
    write_file(out_mrd, |writer| runtime.write_to(writer))
}
