use std::path::Path;

use crate::Error;
use crate::dict_files::FEATURE_DEF_FILE;
use crate::int_templates::IntTemplates;
use crate::part::IntPart;
use crate::template_universe::{UniverseSide, read_bigrams, universe_projections};
use crate::wire::{fnv1a64, get_varint, put_varint, u32_at, u64_at, unzigzag, zigzag};

const MAGIC: &[u8; 4] = b"MRIT";
const VERSION: u32 = 1;
const DIGEST_LEN: usize = 8;

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

    fn word(&mut self) -> Result<u32, Error> {
        Ok(u32_at(self.take(4)?, 0))
    }

    fn varint(&mut self) -> Result<u64, Error> {
        get_varint(self.bytes, &mut self.position)
            .ok_or_else(|| Error::rebuild(self.path, "a varint is too long or the data ends early"))
    }
}

fn expand(
    part: &IntPart,
    left: &UniverseSide,
    right: &UniverseSide,
    path: &Path,
) -> Result<IntPart, Error> {
    let inconsistent = || Error::rebuild(path, "the id definitions do not match the templates");
    let key_map = |projection: &[u16], count: usize, side: &UniverseSide| {
        if projection.len() != side.projection.len() {
            return Err(inconsistent());
        }
        let mut map = vec![u16::MAX; count];
        for (&key, &universe) in projection.iter().zip(&side.projection) {
            if usize::from(key) < count {
                let slot = &mut map[usize::from(key)];
                if usize::from(universe) >= side.count || (*slot != u16::MAX && *slot != universe) {
                    return Err(inconsistent());
                }
                *slot = universe;
            }
        }
        if map.contains(&u16::MAX) || map.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(inconsistent());
        }
        Ok(map)
    };
    let left_map = key_map(&part.left_projection, part.left_count, left)?;
    let right_map = key_map(&part.right_projection, part.right_count, right)?;
    let mut indptr = vec![0u32; right.count + 1];
    for (key, span) in part.indptr.windows(2).enumerate() {
        indptr[usize::from(right_map[key]) + 1] = span[1] - span[0];
    }
    for at in 1..indptr.len() {
        indptr[at] += indptr[at - 1];
    }
    Ok(IntPart {
        left_count: left.count,
        right_count: right.count,
        left_projection: left.projection.clone(),
        right_projection: right.projection.clone(),
        indptr,
        left_index: part
            .left_index
            .iter()
            .map(|&key| left_map[usize::from(key)])
            .collect(),
        weight: part.weight.clone(),
    })
}

fn projection_digest(universe: &[(UniverseSide, UniverseSide)]) -> u64 {
    let mut bytes = Vec::new();
    for side in universe.iter().flat_map(|(left, right)| [left, right]) {
        bytes.extend_from_slice(&(side.count as u32).to_le_bytes());
        for key in &side.projection {
            bytes.extend_from_slice(&key.to_le_bytes());
        }
    }
    fnv1a64(&bytes)
}

fn template_text(bigrams: &[(String, String)]) -> String {
    bigrams
        .iter()
        .map(|(name, body)| format!("{name}\t{body}\n"))
        .collect()
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

#[derive(Default)]
struct Streams {
    pub header: Vec<u8>,
    pub counts: Vec<u8>,
    pub lefts: Vec<u8>,
    pub weights: Vec<u8>,
    pub exception_cells: Vec<u8>,
    pub exception_values: Vec<u8>,
}

impl Streams {
    fn payload(&self) -> Vec<u8> {
        let mut out = self.header.clone();
        for stream in [
            &self.counts,
            &self.lefts,
            &self.weights,
            &self.exception_cells,
            &self.exception_values,
        ] {
            out.extend_from_slice(stream);
        }
        out.extend_from_slice(&fnv1a64(&out).to_le_bytes());
        out
    }
}

pub struct IntContext {
    bigrams: Vec<(String, String)>,
    universe: Vec<(UniverseSide, UniverseSide)>,
}

impl IntContext {
    pub fn load(source_dir: &Path, left_id_def: &str, right_id_def: &str) -> Result<Self, Error> {
        let bigrams = read_bigrams(&source_dir.join(FEATURE_DEF_FILE))?;
        Self::new(bigrams, left_id_def, right_id_def, source_dir)
    }

    fn new(
        bigrams: Vec<(String, String)>,
        left_id_def: &str,
        right_id_def: &str,
        path: &Path,
    ) -> Result<Self, Error> {
        let universe = universe_projections(&bigrams, left_id_def, right_id_def, path)?;
        Ok(Self { bigrams, universe })
    }
}

fn encode_streams(
    templates: &IntTemplates,
    context: &IntContext,
    path: &Path,
) -> Result<Streams, Error> {
    if templates.parts().len() != context.universe.len() {
        return Err(Error::rebuild(
            path,
            "the templates do not match feature.def",
        ));
    }
    let mut streams = Streams::default();
    for (part, (left, right)) in templates.parts().iter().zip(&context.universe) {
        let part = expand(part, left, right, path)?;
        for key in 0..part.right_count {
            let span = part.indptr[key] as usize..part.indptr[key + 1] as usize;
            put_varint(&mut streams.counts, span.len() as u64);
            let mut previous = None;
            for &left_key in &part.left_index[span] {
                let coded =
                    previous.map_or(u64::from(left_key), |p: u16| u64::from(left_key - p - 1));
                put_varint(&mut streams.lefts, coded);
                previous = Some(left_key);
            }
        }
        for &weight in &part.weight {
            put_varint(&mut streams.weights, zigzag(i64::from(weight)));
        }
    }
    let left_size = templates.left_size();
    let mut previous = None;
    for (cell, value) in templates.exceptions() {
        let coded = previous.map_or(u64::from(cell), |p: u32| u64::from(cell - p - 1));
        put_varint(&mut streams.exception_cells, coded);
        previous = Some(cell);
        let computed = templates.computed_cost(
            (cell as usize % left_size) as u16,
            (cell as usize / left_size) as u16,
        );
        put_varint(
            &mut streams.exception_values,
            zigzag(i64::from(value) - i64::from(computed)),
        );
    }
    let text = template_text(&context.bigrams);
    let header = &mut streams.header;
    header.extend_from_slice(MAGIC);
    let lengths = [
        streams.counts.len(),
        streams.lefts.len(),
        streams.weights.len(),
        streams.exception_cells.len(),
        streams.exception_values.len(),
        text.len(),
    ];
    for value in [
        VERSION,
        templates.cost_factor(),
        templates.shift(),
        templates.left_size() as u32,
        templates.right_size() as u32,
        templates.parts().len() as u32,
        templates.exception_count() as u32,
    ]
    .into_iter()
    .chain(lengths.map(|length| length as u32))
    {
        header.extend_from_slice(&value.to_le_bytes());
    }
    header.extend_from_slice(&projection_digest(&context.universe).to_le_bytes());
    header.extend_from_slice(text.as_bytes());
    Ok(streams)
}

pub fn encode_payload(
    templates: &IntTemplates,
    context: &IntContext,
    path: &Path,
) -> Result<Vec<u8>, Error> {
    Ok(encode_streams(templates, context, path)?.payload())
}

pub fn decode_payload(
    payload: &[u8],
    left_id_def: &str,
    right_id_def: &str,
    path: &Path,
) -> Result<IntTemplates, Error> {
    let body_length = payload
        .len()
        .checked_sub(DIGEST_LEN)
        .ok_or_else(|| Error::rebuild(path, "the data is too short"))?;
    let (body, digest) = payload.split_at(body_length);
    if fnv1a64(body) != u64_at(digest, 0) {
        return Err(Error::rebuild(path, "the FNV digest does not match"));
    }
    let mut cursor = Cursor {
        bytes: body,
        position: 0,
        path,
    };
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
    let mut lengths = [0usize; 6];
    for length in &mut lengths {
        *length = cursor.word()? as usize;
    }
    let projection_hash = u64_at(cursor.take(8)?, 0);
    let text = std::str::from_utf8(cursor.take(lengths[5])?)
        .map_err(|_| Error::rebuild(path, "the template text is not UTF-8"))?;
    let bigrams = parse_template_text(text, path)?;
    let context = IntContext::new(bigrams, left_id_def, right_id_def, path)?;
    if context.universe.len() != part_count
        || projection_digest(&context.universe) != projection_hash
    {
        return Err(Error::rebuild(
            path,
            "the id definitions do not match the data",
        ));
    }
    let mut stream = |length: usize| -> Result<Cursor<'_>, Error> {
        Ok(Cursor {
            bytes: cursor.take(length)?,
            position: 0,
            path,
        })
    };
    let mut counts = stream(lengths[0])?;
    let mut lefts = stream(lengths[1])?;
    let mut weights = stream(lengths[2])?;
    let mut exception_cells = stream(lengths[3])?;
    let mut exception_values = stream(lengths[4])?;
    if cursor.position != body.len() {
        return Err(Error::rebuild(path, "the data has trailing bytes"));
    }
    let too_big = || Error::rebuild(path, "a decoded value is out of range");
    let mut parts = Vec::with_capacity(part_count);
    for (left, right) in &context.universe {
        let mut indptr = Vec::with_capacity(right.count + 1);
        indptr.push(0u32);
        let mut left_index = Vec::new();
        for _ in 0..right.count {
            let count = usize::try_from(counts.varint()?).map_err(|_| too_big())?;
            let mut previous: Option<u64> = None;
            for _ in 0..count {
                let coded = lefts.varint()?;
                let key = previous.map_or(Some(coded), |p| p.checked_add(coded)?.checked_add(1));
                let key = key
                    .filter(|&key| key < left.count as u64)
                    .ok_or_else(too_big)?;
                left_index.push(key as u16);
                previous = Some(key);
            }
            indptr.push(u32::try_from(left_index.len()).map_err(|_| too_big())?);
        }
        let weight = (0..left_index.len())
            .map(|_| i32::try_from(unzigzag(weights.varint()?)).map_err(|_| too_big()))
            .collect::<Result<Vec<_>, _>>()?;
        parts.push(
            IntPart {
                left_count: left.count,
                right_count: right.count,
                left_projection: left.projection.clone(),
                right_projection: right.projection.clone(),
                indptr,
                left_index,
                weight,
            }
            .compacted(),
        );
    }
    let plain = IntTemplates::new(cost_factor, shift, left_size, right_size, parts, Vec::new())?;
    let mut exceptions = Vec::with_capacity(exception_count);
    let mut previous: Option<u64> = None;
    for _ in 0..exception_count {
        let coded = exception_cells.varint()?;
        let cell = previous.map_or(Some(coded), |p| p.checked_add(coded)?.checked_add(1));
        let cell = cell
            .filter(|&cell| cell < (left_size * right_size) as u64)
            .ok_or_else(too_big)?;
        previous = Some(cell);
        let (column, row) = (cell as usize % left_size, cell as usize / left_size);
        let computed = plain.computed_cost(column as u16, row as u16);
        let value = i64::from(computed) + unzigzag(exception_values.varint()?);
        exceptions.push((cell as u32, i16::try_from(value).map_err(|_| too_big())?));
    }
    if counts.position != counts.bytes.len()
        || lefts.position != lefts.bytes.len()
        || weights.position != weights.bytes.len()
        || exception_cells.position != exception_cells.bytes.len()
        || exception_values.position != exception_values.bytes.len()
    {
        return Err(Error::rebuild(path, "a stream has trailing bytes"));
    }
    plain.with_exceptions(exceptions)
}
