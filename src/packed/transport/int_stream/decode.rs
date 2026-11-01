use std::path::Path;

use super::{IntContext, MAGIC, VERSION, projection_digest};
use crate::Error;
use crate::hot_shape::MAX_KEYS;
use crate::int_templates::IntTemplates;
use crate::part::IntPart;
use crate::template_universe::UniverseSide;
use crate::wire::{fnv1a64, get_varint, u32_at, u64_at, unzigzag};

const DIGEST_LEN: usize = 8;
const STREAM_COUNT: usize = 6;
const TEXT_STREAM: usize = 5;
const MAX_TEMPLATES: usize = 256;

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
    path: &'a Path,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let end = self
            .position
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| Error::rebuild(self.path, "the data ends early"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn remaining_bytes(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn word(&mut self) -> Result<u32, Error> {
        Ok(u32_at(self.take(4)?, 0))
    }

    fn varint(&mut self) -> Result<u64, Error> {
        get_varint(self.bytes, &mut self.position)
            .ok_or_else(|| Error::rebuild(self.path, "a varint is too long or the data ends early"))
    }
}

fn parse_template_text(text: &str, path: &Path) -> Result<Vec<(String, String)>, Error> {
    text.split_terminator('\n')
        .map(|line| {
            line.split_once('\t')
                .map(|(name, body)| (name.to_string(), body.to_string()))
                .ok_or_else(|| Error::rebuild(path, "a template line has no tab"))
        })
        .collect()
}

struct Header {
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    part_count: usize,
    exception_count: usize,
    lengths: [usize; STREAM_COUNT],
    projection_hash: u64,
}

fn too_big(path: &Path) -> Error {
    Error::rebuild(path, "a decoded value is out of range")
}

fn verified_body<'a>(payload: &'a [u8], path: &Path) -> Result<&'a [u8], Error> {
    let body_length = payload
        .len()
        .checked_sub(DIGEST_LEN)
        .ok_or_else(|| Error::rebuild(path, "the data is too short"))?;
    let (body, digest) = payload.split_at(body_length);
    if fnv1a64(body) != u64_at(digest, 0) {
        return Err(Error::rebuild(path, "the FNV digest does not match"));
    }
    Ok(body)
}

fn read_header(cursor: &mut Cursor, path: &Path) -> Result<Header, Error> {
    if cursor.take(MAGIC.len())? != MAGIC || cursor.word()? != VERSION {
        return Err(Error::rebuild(
            path,
            "not an integer matrix of this version",
        ));
    }
    let cost_factor = cursor.word()?;
    let shift = cursor.word()?;
    let left_size = cursor.word()? as usize;
    let right_size = cursor.word()? as usize;
    let part_count = cursor.word()? as usize;
    let exception_count = cursor.word()? as usize;
    let mut lengths = [0usize; STREAM_COUNT];
    for length in &mut lengths {
        *length = cursor.word()? as usize;
    }
    if !(1..=MAX_KEYS).contains(&left_size) || !(1..=MAX_KEYS).contains(&right_size) {
        return Err(too_big(path));
    }
    if part_count > MAX_TEMPLATES || exception_count > left_size * right_size {
        return Err(too_big(path));
    }
    let projection_hash = u64_at(cursor.take(8)?, 0);
    Ok(Header {
        cost_factor,
        shift,
        left_size,
        right_size,
        part_count,
        exception_count,
        lengths,
        projection_hash,
    })
}

fn read_context(
    cursor: &mut Cursor,
    header: &Header,
    left_id_def: &str,
    right_id_def: &str,
    path: &Path,
) -> Result<IntContext, Error> {
    let text = std::str::from_utf8(cursor.take(header.lengths[TEXT_STREAM])?)
        .map_err(|_| Error::rebuild(path, "the template text is not UTF-8"))?;
    let bigrams = parse_template_text(text, path)?;
    if bigrams.len() != header.part_count {
        return Err(Error::rebuild(
            path,
            "the template count does not match the data",
        ));
    }
    let context = IntContext::new(bigrams, left_id_def, right_id_def, path)?;
    if projection_digest(&context.universe) != header.projection_hash {
        return Err(Error::rebuild(
            path,
            "the id definitions do not match the data",
        ));
    }
    Ok(context)
}

struct Inputs<'a> {
    counts: Cursor<'a>,
    lefts: Cursor<'a>,
    weights: Cursor<'a>,
    exception_cells: Cursor<'a>,
    exception_values: Cursor<'a>,
}

impl<'a> Inputs<'a> {
    fn split(cursor: &mut Cursor<'a>, lengths: &[usize], path: &'a Path) -> Result<Self, Error> {
        let mut stream = |length: usize| -> Result<Cursor<'a>, Error> {
            Ok(Cursor {
                bytes: cursor.take(length)?,
                position: 0,
                path,
            })
        };
        let inputs = Self {
            counts: stream(lengths[0])?,
            lefts: stream(lengths[1])?,
            weights: stream(lengths[2])?,
            exception_cells: stream(lengths[3])?,
            exception_values: stream(lengths[4])?,
        };
        if cursor.position != cursor.bytes.len() {
            return Err(Error::rebuild(path, "the data has trailing bytes"));
        }
        Ok(inputs)
    }

    fn check_ends(&self, path: &Path) -> Result<(), Error> {
        let streams = [
            &self.counts,
            &self.lefts,
            &self.weights,
            &self.exception_cells,
            &self.exception_values,
        ];
        if streams
            .iter()
            .all(|stream| stream.position == stream.bytes.len())
        {
            Ok(())
        } else {
            Err(Error::rebuild(path, "a stream has trailing bytes"))
        }
    }
}

fn next_key(previous: Option<u64>, coded: u64) -> Option<u64> {
    previous.map_or(Some(coded), |p| p.checked_add(coded)?.checked_add(1))
}

fn decode_part(
    left: &UniverseSide,
    right: &UniverseSide,
    inputs: &mut Inputs,
    path: &Path,
) -> Result<IntPart, Error> {
    let mut indptr = Vec::with_capacity(right.count + 1);
    indptr.push(0u32);
    let mut left_index = Vec::new();
    for _ in 0..right.count {
        let count = usize::try_from(inputs.counts.varint()?).map_err(|_| too_big(path))?;
        let mut previous: Option<u64> = None;
        for _ in 0..count {
            let key = next_key(previous, inputs.lefts.varint()?)
                .filter(|&key| key < left.count as u64)
                .ok_or_else(|| too_big(path))?;
            left_index.push(key as u16);
            previous = Some(key);
        }
        indptr.push(u32::try_from(left_index.len()).map_err(|_| too_big(path))?);
    }
    let weight = (0..left_index.len())
        .map(|_| i32::try_from(unzigzag(inputs.weights.varint()?)).map_err(|_| too_big(path)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(IntPart {
        left_count: left.count,
        right_count: right.count,
        left_projection: left.projection.clone(),
        right_projection: right.projection.clone(),
        indptr,
        left_index,
        weight,
    }
    .compacted())
}

fn decode_exceptions(
    plain: &IntTemplates,
    header: &Header,
    inputs: &mut Inputs,
    path: &Path,
) -> Result<(Vec<u32>, Vec<i16>), Error> {
    let left_size = header.left_size;
    if header.exception_count > inputs.exception_cells.remaining_bytes() {
        return Err(too_big(path));
    }
    let mut cells = Vec::with_capacity(header.exception_count);
    let mut values = Vec::with_capacity(header.exception_count);
    let mut previous: Option<u64> = None;
    for _ in 0..header.exception_count {
        let cell = next_key(previous, inputs.exception_cells.varint()?)
            .filter(|&cell| cell < (left_size * header.right_size) as u64)
            .ok_or_else(|| too_big(path))?;
        previous = Some(cell);
        let (column, row) = (cell as usize % left_size, cell as usize / left_size);
        let computed = plain.computed_cost(column as u16, row as u16);
        let value = i64::from(computed)
            .checked_add(unzigzag(inputs.exception_values.varint()?))
            .ok_or_else(|| too_big(path))?;
        cells.push(cell as u32);
        values.push(i16::try_from(value).map_err(|_| too_big(path))?);
    }
    Ok((cells, values))
}

pub fn decode_payload(
    payload: &[u8],
    left_id_def: &str,
    right_id_def: &str,
    path: &Path,
) -> Result<IntTemplates, Error> {
    let body = verified_body(payload, path)?;
    let mut cursor = Cursor {
        bytes: body,
        position: 0,
        path,
    };
    let header = read_header(&mut cursor, path)?;
    let context = read_context(&mut cursor, &header, left_id_def, right_id_def, path)?;
    let mut inputs = Inputs::split(&mut cursor, &header.lengths, path)?;
    let mut parts = Vec::with_capacity(header.part_count);
    for (left, right) in &context.universe {
        parts.push(decode_part(left, right, &mut inputs, path)?);
    }
    let plain = IntTemplates::new(
        header.cost_factor,
        header.shift,
        header.left_size,
        header.right_size,
        parts,
        Vec::new(),
        Vec::new(),
    )?;
    let (cells, values) = decode_exceptions(&plain, &header, &mut inputs, path)?;
    inputs.check_ends(path)?;
    plain.with_exceptions(cells, values)
}
