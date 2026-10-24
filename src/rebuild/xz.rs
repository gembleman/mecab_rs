
use std::io::{Read, Write};
use std::path::Path;

use lzma_rust2::{XzOptions, XzReader, XzWriter};

use super::{bad, io_error, write_matrix_files};
use crate::build::write_file;
use crate::{ConnectionTemplates, Error, TemplatePart};

const MAGIC: &[u8; 4] = b"MRCT";
const VERSION: u32 = 2;
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const DIGEST_LEN: usize = 8;
const WEIGHT_SCALE: f64 = 1e16;
const NICE_LEN: u32 = 273;
const DEPTH_LIMIT: i32 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    PerTemplate = 0,
    ByField = 1,
}

impl Layout {
    pub const ALL: [Layout; 2] = [Layout::PerTemplate, Layout::ByField];
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |state, &byte| {
        (state ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

fn fixed_point(weight: f64) -> i64 {
    (weight * WEIGHT_SCALE).round() as i64
}

fn restore(fixed: i64) -> f64 {
    fixed as f64 / WEIGHT_SCALE
}

fn is_exception(weight: f64) -> bool {
    restore(fixed_point(weight)).to_bits() != weight.to_bits()
}

fn put_meta(out: &mut Vec<u8>, part: &TemplatePart) {
    for value in [part.left_count, part.right_count, part.weight.len()] {
        out.extend_from_slice(&(value as u32).to_le_bytes());
    }
}

fn put_projections(out: &mut Vec<u8>, part: &TemplatePart) {
    part.left_projection
        .iter()
        .chain(&part.right_projection)
        .for_each(|value| out.extend_from_slice(&value.to_le_bytes()));
}

fn put_indptr(out: &mut Vec<u8>, part: &TemplatePart) {
    part.indptr
        .iter()
        .for_each(|value| out.extend_from_slice(&value.to_le_bytes()));
}

fn put_left_index(out: &mut Vec<u8>, part: &TemplatePart) {
    part.left_index
        .iter()
        .for_each(|value| out.extend_from_slice(&value.to_le_bytes()));
}

fn put_weights(
    out: &mut Vec<u8>,
    part: &TemplatePart,
    offset: usize,
    exceptions: &mut Vec<(u32, u64)>,
) {
    for (index, &weight) in part.weight.iter().enumerate() {
        if is_exception(weight) {
            exceptions.push(((offset + index) as u32, weight.to_bits()));
        }
        out.extend_from_slice(&fixed_point(weight).to_le_bytes());
    }
}

fn payload(templates: &ConnectionTemplates, layout: Layout) -> Vec<u8> {
    let parts = templates.parts();
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    for value in [
        VERSION,
        templates.cost_factor() as u32,
        templates.left_size() as u32,
        templates.right_size() as u32,
        parts.len() as u32,
        layout as u32,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let offsets: Vec<usize> = parts
        .iter()
        .scan(0, |total, part| {
            let offset = *total;
            *total += part.weight.len();
            Some(offset)
        })
        .collect();
    let mut exceptions = Vec::new();
    match layout {
        Layout::PerTemplate => {
            for (part, &offset) in parts.iter().zip(&offsets) {
                put_meta(&mut out, part);
                put_projections(&mut out, part);
                put_indptr(&mut out, part);
                put_left_index(&mut out, part);
                put_weights(&mut out, part, offset, &mut exceptions);
            }
        }
        Layout::ByField => {
            parts.iter().for_each(|part| put_meta(&mut out, part));
            parts
                .iter()
                .for_each(|part| put_projections(&mut out, part));
            parts.iter().for_each(|part| put_indptr(&mut out, part));
            parts.iter().for_each(|part| put_left_index(&mut out, part));
            for (part, &offset) in parts.iter().zip(&offsets) {
                put_weights(&mut out, part, offset, &mut exceptions);
            }
        }
    }
    out.extend_from_slice(&(exceptions.len() as u32).to_le_bytes());
    for (index, bits) in exceptions {
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
    }
    let digest = fnv1a64(&out);
    out.extend_from_slice(&digest.to_le_bytes());
    out
}

fn compress(payload: &[u8], path: &Path) -> Result<Vec<u8>, Error> {
    let mut options = XzOptions::with_preset(9);
    options.lzma_options.nice_len = NICE_LEN;
    options.lzma_options.depth_limit = DEPTH_LIMIT;
    options.lzma_options.dict_size = options
        .lzma_options
        .dict_size
        .max(u32::try_from(payload.len()).unwrap_or(u32::MAX));
    let failed = |error: std::io::Error| Error::Rebuild {
        path: path.to_path_buf(),
        reason: format!("xz: {error}"),
    };
    let mut writer = XzWriter::new(Vec::new(), options).map_err(failed)?;
    writer.write_all(payload).map_err(failed)?;
    writer.finish().map_err(failed)
}

pub fn pack(templates: &ConnectionTemplates, path: &Path) -> Result<u64, Error> {
    let packed = std::thread::scope(|scope| {
        let handles = Layout::ALL
            .map(|layout| scope.spawn(move || compress(&payload(templates, layout), path)));
        handles.map(|handle| {
            handle
                .join()
                .expect("the compression thread does not panic")
        })
    });
    let [first, second] = packed;
    let (first, second) = (first?, second?);
    let smaller = if second.len() < first.len() {
        second
    } else {
        first
    };
    write_file(path, |writer| writer.write_all(&smaller))?;
    Ok(std::fs::metadata(path).map_err(io_error(path))?.len())
}

fn decompress(path: &Path) -> Result<Vec<u8>, Error> {
    let file = std::fs::File::open(path).map_err(io_error(path))?;
    let mut payload = Vec::new();
    XzReader::new(std::io::BufReader::new(file), false)
        .read_to_end(&mut payload)
        .map_err(|error| bad(path, format!("xz: {error}")))?;
    Ok(payload)
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    path: &'a Path,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let end = self
            .position
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| bad(self.path, "the data ends early"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn count(&mut self) -> Result<usize, Error> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
    }

    fn array<T, const WIDTH: usize>(
        &mut self,
        length: usize,
        decode: fn([u8; WIDTH]) -> T,
    ) -> Result<Vec<T>, Error> {
        let bytes = self.take(
            length
                .checked_mul(WIDTH)
                .ok_or_else(|| bad(self.path, "an array is too long"))?,
        )?;
        Ok(bytes
            .as_chunks::<WIDTH>()
            .0
            .iter()
            .map(|chunk| decode(*chunk))
            .collect())
    }

    fn meta(&mut self) -> Result<(TemplatePart, usize), Error> {
        let left_count = self.count()?;
        let right_count = self.count()?;
        let nnz = self.count()?;
        let part = TemplatePart {
            left_count,
            right_count,
            left_projection: Vec::new(),
            right_projection: Vec::new(),
            indptr: Vec::new(),
            left_index: Vec::new(),
            weight: Vec::new(),
        };
        Ok((part, nnz))
    }

    fn projections(&mut self, part: &mut TemplatePart, sizes: (usize, usize)) -> Result<(), Error> {
        part.left_projection = self.array(sizes.0, u16::from_le_bytes)?;
        part.right_projection = self.array(sizes.1, u16::from_le_bytes)?;
        Ok(())
    }

    fn indptr(&mut self, part: &mut TemplatePart) -> Result<(), Error> {
        part.indptr = self.array(part.right_count + 1, u32::from_le_bytes)?;
        Ok(())
    }

    fn left_index(&mut self, part: &mut TemplatePart, nnz: usize) -> Result<(), Error> {
        part.left_index = self.array(nnz, u16::from_le_bytes)?;
        Ok(())
    }

    fn weights(&mut self, part: &mut TemplatePart, nnz: usize) -> Result<(), Error> {
        part.weight = self.array(nnz, |bytes| restore(i64::from_le_bytes(bytes)))?;
        Ok(())
    }
}

fn parse_payload(payload: &[u8], path: &Path) -> Result<ConnectionTemplates, Error> {
    let body_length = payload
        .len()
        .checked_sub(DIGEST_LEN)
        .ok_or_else(|| bad(path, "the data is too short"))?;
    let (body, digest) = payload.split_at(body_length);
    if fnv1a64(body) != u64::from_le_bytes(digest.try_into().unwrap()) {
        return Err(bad(path, "the FNV digest does not match"));
    }
    let mut reader = Reader {
        bytes: body,
        position: 0,
        path,
    };
    if reader.take(MAGIC.len())? != MAGIC {
        return Err(bad(path, "the magic is not MRCT"));
    }
    if reader.count()? != VERSION as usize {
        return Err(bad(path, "unsupported version"));
    }
    let cost_factor = f64::from(u32::try_from(reader.count()?).unwrap_or(u32::MAX));
    let left_size = reader.count()?;
    let right_size = reader.count()?;
    if left_size > usize::from(u16::MAX) || right_size > usize::from(u16::MAX) {
        return Err(bad(path, "the matrix size exceeds u16"));
    }
    let part_count = reader.count()?;
    let layout = match reader.count()? {
        0 => Layout::PerTemplate,
        1 => Layout::ByField,
        _ => return Err(bad(path, "unknown layout")),
    };
    let sizes = (left_size, right_size);
    let mut parts = Vec::new();
    let mut entries = Vec::new();
    match layout {
        Layout::PerTemplate => {
            for _ in 0..part_count {
                let (mut part, nnz) = reader.meta()?;
                reader.projections(&mut part, sizes)?;
                reader.indptr(&mut part)?;
                reader.left_index(&mut part, nnz)?;
                reader.weights(&mut part, nnz)?;
                parts.push(part);
                entries.push(nnz);
            }
        }
        Layout::ByField => {
            for _ in 0..part_count {
                let (part, nnz) = reader.meta()?;
                parts.push(part);
                entries.push(nnz);
            }
            for part in &mut parts {
                reader.projections(part, sizes)?;
            }
            for part in &mut parts {
                reader.indptr(part)?;
            }
            for (part, &nnz) in parts.iter_mut().zip(&entries) {
                reader.left_index(part, nnz)?;
            }
            for (part, &nnz) in parts.iter_mut().zip(&entries) {
                reader.weights(part, nnz)?;
            }
        }
    }
    let exception_count = reader.count()?;
    let total: usize = entries.iter().sum();
    let mut starts = Vec::with_capacity(parts.len());
    let mut start = 0;
    for &nnz in &entries {
        starts.push(start);
        start += nnz;
    }
    for _ in 0..exception_count {
        let index = reader.count()?;
        let bits = u64::from_le_bytes(reader.take(8)?.try_into().unwrap());
        if index >= total {
            return Err(bad(path, "an exception index is out of range"));
        }
        let owner = starts.partition_point(|&begin| begin <= index) - 1;
        parts[owner].weight[index - starts[owner]] = f64::from_bits(bits);
    }
    if reader.position != body.len() {
        return Err(bad(path, "the data has trailing bytes"));
    }
    for (part, &nnz) in parts.iter().zip(&entries) {
        let consistent = part.indptr.last().copied() == Some(nnz as u32)
            && part
                .left_projection
                .iter()
                .all(|&key| usize::from(key) <= part.left_count)
            && part
                .right_projection
                .iter()
                .all(|&key| usize::from(key) <= part.right_count)
            && part
                .left_index
                .iter()
                .all(|&key| usize::from(key) < part.left_count);
        if !consistent {
            return Err(bad(path, "a template index is out of range"));
        }
    }
    ConnectionTemplates::new(cost_factor, left_size, right_size, parts)
}

fn read_pack(path: &Path) -> Result<ConnectionTemplates, Error> {
    parse_payload(&decompress(path)?, path)
}

pub fn unpack(
    pack: &Path,
    dict_dir: &Path,
    hot: Option<(usize, usize)>,
    threads: usize,
) -> Result<(), Error> {
    let templates = read_pack(pack)?;
    write_matrix_files(&templates, dict_dir, hot, threads)
}
