
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::bad;
#[cfg(feature = "xz")]
use super::io_error;
use super::templates::{UniverseSide, read_bigrams, universe_projections};
use crate::dictionary::map_file;
use crate::templates::ExactRows;
use crate::{ConnectionTemplates, Error, IntPart, IntTemplates};

const MAGIC: &[u8; 4] = b"MRIT";
const VERSION: u32 = 1;
const DIGEST_LEN: usize = 8;
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
pub use crate::int_templates::MAX_SHIFT;
pub const DEFAULT_SHIFTS: std::ops::RangeInclusive<u32> = 16..=26;
pub const RUNTIME_SHIFT: u32 = 20;

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |state, &byte| {
        (state ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

fn integer_cost_factor(templates: &ConnectionTemplates, path: &Path) -> Result<u32, Error> {
    let factor = templates.cost_factor();
    if factor >= 1.0 && factor <= f64::from(crate::int_templates::MAX_COST_FACTOR) {
        let integer = factor as u32;
        if f64::from(integer) == factor {
            return Ok(integer);
        }
    }
    Err(bad(path, "the cost-factor is not a usable integer"))
}

pub fn quantize(
    templates: &ConnectionTemplates,
    shift: u32,
    path: &Path,
) -> Result<IntTemplates, Error> {
    let scale = 2f64.powi(shift as i32);
    let mut parts = Vec::with_capacity(templates.parts().len());
    for part in templates.parts() {
        let mut indptr = vec![0u32];
        let mut left_index = Vec::new();
        let mut weight = Vec::new();
        for span in part.indptr.windows(2) {
            for at in span[0] as usize..span[1] as usize {
                let rounded = (part.weight[at] * scale).round();
                if rounded.is_nan() || rounded.abs() > f64::from(i32::MAX) {
                    return Err(bad(path, "a weight does not fit the integer grid"));
                }
                if rounded != 0.0 {
                    left_index.push(part.left_index[at]);
                    weight.push(rounded as i32);
                }
            }
            indptr.push(left_index.len() as u32);
        }
        parts.push(
            IntPart {
                left_count: part.left_count,
                right_count: part.right_count,
                left_projection: part.left_projection.clone(),
                right_projection: part.right_projection.clone(),
                indptr,
                left_index,
                weight,
            }
            .compacted(),
        );
    }
    IntTemplates::new(
        integer_cost_factor(templates, path)?,
        shift,
        templates.left_size(),
        templates.right_size(),
        parts,
        Vec::new(),
    )
}

pub fn find_exceptions(
    exact: &ConnectionTemplates,
    reference: Option<&Path>,
    candidates: &[IntTemplates],
    threads: usize,
) -> Result<Vec<Vec<(u32, i16)>>, Error> {
    let (left_size, right_size) = (exact.left_size(), exact.right_size());
    if u32::try_from(left_size * right_size).is_err()
        || candidates
            .iter()
            .any(|c| c.left_size() != left_size || c.right_size() != right_size)
    {
        return Err(Error::Corrupt("the candidates do not fit the matrix"));
    }
    let map = reference.map(map_file).transpose()?;
    if let (Some(map), Some(path)) = (&map, reference)
        && (map.len() != 4 + 2 * left_size * right_size
            || usize::from(u16::from_le_bytes([map[0], map[1]])) != left_size
            || usize::from(u16::from_le_bytes([map[2], map[3]])) != right_size)
    {
        return Err(bad(
            path,
            "the matrix does not have the size of the templates",
        ));
    }
    let views: Vec<_> = candidates.iter().map(IntTemplates::view).collect();
    let next_row = AtomicUsize::new(0);
    let found = Mutex::new(vec![Vec::new(); candidates.len()]);
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                let mut exact_rows = ExactRows::new(exact);
                let mut scratch = views.iter().map(|view| view.scratch()).collect::<Vec<_>>();
                let mut real = vec![0i16; left_size];
                let mut computed = vec![0i16; left_size];
                let mut local: Vec<Vec<(u32, i16)>> = vec![Vec::new(); views.len()];
                loop {
                    let row = next_row.fetch_add(1, Ordering::Relaxed);
                    if row >= right_size {
                        break;
                    }
                    match &map {
                        Some(map) => {
                            let bytes = &map[4 + 2 * row * left_size..][..2 * left_size];
                            for (cost, chunk) in real.iter_mut().zip(bytes.as_chunks::<2>().0) {
                                *cost = i16::from_le_bytes(*chunk);
                            }
                        }
                        None => exact_rows.render(row, &mut real),
                    }
                    for ((view, scratch), list) in views.iter().zip(&mut scratch).zip(&mut local) {
                        view.render_computed_row(row, scratch, &mut computed);
                        for (column, (&a, &b)) in computed.iter().zip(&real).enumerate() {
                            if a != b {
                                list.push(((row * left_size + column) as u32, b));
                            }
                        }
                    }
                }
                let mut found = found.lock().expect("an exception worker panicked");
                for (all, list) in found.iter_mut().zip(local) {
                    all.extend(list);
                }
            });
        }
    });
    let mut found = found.into_inner().expect("an exception worker panicked");
    for list in &mut found {
        list.sort_unstable_by_key(|&(cell, _)| cell);
    }
    Ok(found)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

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
            .ok_or_else(|| bad(self.path, "the data ends early"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn word(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn varint(&mut self) -> Result<u64, Error> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)?[0];
            value |= u64::from(byte & 0x7F) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err(bad(self.path, "a varint is too long"))
    }
}

fn expand(
    part: &IntPart,
    left: &UniverseSide,
    right: &UniverseSide,
    path: &Path,
) -> Result<IntPart, Error> {
    let inconsistent = || bad(path, "the id definitions do not match the templates");
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
                .ok_or_else(|| bad(path, "a template line has no tab"))
        })
        .collect()
}

#[derive(Default)]
pub struct Streams {
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
        let bigrams = read_bigrams(&source_dir.join("feature.def"))?;
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

pub fn encode_streams(
    templates: &IntTemplates,
    context: &IntContext,
    path: &Path,
) -> Result<Streams, Error> {
    if templates.parts().len() != context.universe.len() {
        return Err(bad(path, "the templates do not match feature.def"));
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
        .ok_or_else(|| bad(path, "the data is too short"))?;
    let (body, digest) = payload.split_at(body_length);
    if fnv1a64(body) != u64::from_le_bytes(digest.try_into().unwrap()) {
        return Err(bad(path, "the FNV digest does not match"));
    }
    let mut cursor = Cursor {
        bytes: body,
        position: 0,
        path,
    };
    if cursor.take(MAGIC.len())? != MAGIC || cursor.word()? != VERSION {
        return Err(bad(path, "not an integer matrix of this version"));
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
    let projection_hash = u64::from_le_bytes(cursor.take(8)?.try_into().unwrap());
    let text = std::str::from_utf8(cursor.take(lengths[5])?)
        .map_err(|_| bad(path, "the template text is not UTF-8"))?;
    let bigrams = parse_template_text(text, path)?;
    let context = IntContext::new(bigrams, left_id_def, right_id_def, path)?;
    if context.universe.len() != part_count
        || projection_digest(&context.universe) != projection_hash
    {
        return Err(bad(path, "the id definitions do not match the data"));
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
        return Err(bad(path, "the data has trailing bytes"));
    }
    let too_big = || bad(path, "a decoded value is out of range");
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
        return Err(bad(path, "a stream has trailing bytes"));
    }
    plain.with_exceptions(exceptions)
}

fn log2_binomial(n: f64, k: f64) -> f64 {
    if k <= 0.0 || k >= n {
        return 0.0;
    }
    (ln_gamma(n + 1.0) - ln_gamma(k + 1.0) - ln_gamma(n - k + 1.0)) / std::f64::consts::LN_2
}

fn ln_gamma(mut x: f64) -> f64 {
    let mut shift = 0.0;
    while x < 15.0 {
        shift -= x.ln();
        x += 1.0;
    }
    let series = 1.0 / (12.0 * x) - 1.0 / (360.0 * x.powi(3)) + 1.0 / (1260.0 * x.powi(5));
    shift + (x - 0.5) * x.ln() - x + 0.5 * (2.0 * std::f64::consts::PI).ln() + series
}

fn adaptive_bits(values: impl Iterator<Item = i64>) -> f64 {
    let mut counts: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
    let (mut total, mut bits) = (0u64, 0.0);
    for value in values {
        let seen = counts.get(&value).copied().unwrap_or(0);
        let distinct = counts.len() as f64;
        if seen > 0 {
            bits -= (seen as f64 / (total as f64 + distinct)).log2();
        } else {
            if total > 0 {
                bits -= (distinct / (total as f64 + distinct)).log2();
            }
            let folded = zigzag(value);
            bits += f64::from(2 * (u64::BITS - (folded + 1).leading_zeros() - 1) + 1);
        }
        *counts.entry(value).or_insert(0) += 1;
        total += 1;
    }
    bits
}

#[derive(Debug, Clone, Copy)]
pub struct IdealBytes {
    pub pattern: f64,
    pub weights: f64,
    pub exceptions: f64,
}

impl IdealBytes {
    pub fn total(&self) -> f64 {
        self.pattern + self.weights + self.exceptions
    }
}

pub fn ideal_bytes(templates: &IntTemplates) -> IdealBytes {
    let pattern: f64 = templates
        .parts()
        .iter()
        .map(|part| {
            log2_binomial(
                (part.left_count * part.right_count) as f64,
                part.weight.len() as f64,
            )
        })
        .sum();
    let weights = adaptive_bits(
        templates
            .parts()
            .iter()
            .flat_map(|part| part.weight.iter().map(|&weight| i64::from(weight))),
    );
    let cells = (templates.left_size() * templates.right_size()) as f64;
    let count = templates.exception_count();
    let mut corrections: BTreeMap<i64, u64> = BTreeMap::new();
    for (cell, value) in templates.exceptions() {
        let left_size = templates.left_size();
        let computed = templates.computed_cost(
            (cell as usize % left_size) as u16,
            (cell as usize / left_size) as u16,
        );
        *corrections
            .entry(i64::from(value) - i64::from(computed))
            .or_insert(0) += 1;
    }
    let entropy: f64 = corrections
        .values()
        .map(|&seen| {
            let share = seen as f64 / count as f64;
            -share * share.log2()
        })
        .sum();
    let exceptions = log2_binomial(cells, count as f64) + count as f64 * (entropy + 0.5);
    IdealBytes {
        pattern: pattern / 8.0,
        weights: weights / 8.0,
        exceptions: exceptions / 8.0,
    }
}

#[cfg(feature = "xz")]
#[derive(Debug, Clone, Copy)]
pub struct PieceSizes {
    pub header: (usize, usize),
    pub pattern: (usize, usize),
    pub weights: (usize, usize),
    pub exceptions: (usize, usize),
    pub total: (usize, usize),
}

#[cfg(feature = "xz")]
pub fn piece_sizes(streams: &Streams, path: &Path) -> Result<PieceSizes, Error> {
    let measure = |bytes: &[u8]| -> Result<(usize, usize), Error> {
        Ok((bytes.len(), super::xz::compress(bytes, path)?.len()))
    };
    let join = |a: &[u8], b: &[u8]| [a, b].concat();
    Ok(PieceSizes {
        header: measure(&streams.header)?,
        pattern: measure(&join(&streams.counts, &streams.lefts))?,
        weights: measure(&streams.weights)?,
        exceptions: measure(&join(&streams.exception_cells, &streams.exception_values))?,
        total: measure(&streams.payload())?,
    })
}

#[cfg(feature = "xz")]
#[derive(Debug, Clone)]
pub struct ShiftReport {
    pub shift: u32,
    pub entries: usize,
    pub exceptions: usize,
    pub sizes: PieceSizes,
    pub ideal: IdealBytes,
}

#[cfg(feature = "xz")]
#[allow(clippy::too_many_arguments)]
pub fn pack_int(
    templates: &ConnectionTemplates,
    source_dir: &Path,
    left_id_def: &str,
    right_id_def: &str,
    shifts: &[u32],
    reference: Option<&Path>,
    threads: usize,
    out: &Path,
    log: &mut dyn FnMut(&str),
) -> Result<(Vec<ShiftReport>, usize), Error> {
    let started = std::time::Instant::now();
    let context = IntContext::load(source_dir, left_id_def, right_id_def)?;
    let plain = shifts
        .iter()
        .map(|&shift| quantize(templates, shift, out))
        .collect::<Result<Vec<_>, _>>()?;
    log(&format!(
        "quantized: {:.1} s",
        started.elapsed().as_secs_f64()
    ));
    let exceptions = find_exceptions(templates, reference, &plain, threads)?;
    log(&format!(
        "exceptions found: {:.1} s",
        started.elapsed().as_secs_f64()
    ));
    let mut reports = Vec::new();
    let mut best: Option<(usize, Vec<u8>)> = None;
    for ((plain, exceptions), &shift) in plain.iter().zip(exceptions).zip(shifts) {
        let with = plain.with_exceptions(exceptions)?;
        let streams = encode_streams(&with, &context, out)?;
        let sizes = piece_sizes(&streams, out)?;
        let report = ShiftReport {
            shift,
            entries: with.parts().iter().map(|part| part.weight.len()).sum(),
            exceptions: with.exception_count(),
            sizes,
            ideal: ideal_bytes(&with),
        };
        log(&format!(
            "shift {shift}: {} entries, {} exceptions, xz {} bytes, ideal {:.0} bytes ({:.1} s)",
            report.entries,
            report.exceptions,
            sizes.total.1,
            report.ideal.total(),
            started.elapsed().as_secs_f64()
        ));
        if best.as_ref().is_none_or(|(size, _)| sizes.total.1 < *size) {
            let packed = super::xz::compress(&streams.payload(), out)?;
            best = Some((packed.len(), packed));
            reports.push(report);
            continue;
        }
        reports.push(report);
    }
    let (_, packed) = best.ok_or_else(|| bad(out, "no grid given"))?;
    let chosen = reports
        .iter()
        .enumerate()
        .min_by_key(|(_, report)| report.sizes.total.1)
        .map_or(0, |(index, _)| index);
    crate::build::write_file(out, |writer| std::io::Write::write_all(writer, &packed))?;
    Ok((reports, chosen))
}

#[cfg(feature = "xz")]
pub fn read_int_pack(
    path: &Path,
    left_id_def: &str,
    right_id_def: &str,
) -> Result<IntTemplates, Error> {
    decode_payload(
        &super::xz::decompress(path)?,
        left_id_def,
        right_id_def,
        path,
    )
}

pub fn exact_int_templates(
    templates: &ConnectionTemplates,
    shift: u32,
    threads: usize,
    path: &Path,
) -> Result<IntTemplates, Error> {
    let plain = quantize(templates, shift, path)?;
    let exceptions = find_exceptions(templates, None, std::slice::from_ref(&plain), threads)?
        .pop()
        .expect("one candidate gives one exception list");
    plain.with_exceptions(exceptions)
}

pub fn write_int_files(
    templates: &IntTemplates,
    dict_dir: &Path,
    hot: Option<(usize, usize)>,
    threads: usize,
) -> Result<(), Error> {
    use crate::build::write_file;
    let (hot_left, hot_right) = hot.unwrap_or((super::DEFAULT_HOT_LEFT, super::DEFAULT_HOT_RIGHT));
    let (hot_left, hot_right) = (
        hot_left.min(templates.left_size()),
        hot_right.min(templates.right_size()),
    );
    super::templates::remove_stale(dict_dir, "matrix.tpl")?;
    write_file(&dict_dir.join("matrix.itp"), |writer| {
        templates.write_templates(writer)
    })?;
    write_file(&dict_dir.join("matrix.hot"), |writer| {
        templates.write_hot_matrix(hot_left, hot_right, threads, writer)
    })
}

#[cfg(feature = "xz")]
pub fn unpack_int(
    pack: &Path,
    id_defs: &Path,
    dict_dir: &Path,
    hot: Option<(usize, usize)>,
    threads: usize,
) -> Result<(), Error> {
    let read = |name: &str| {
        let path = id_defs.join(name);
        std::fs::read_to_string(&path).map_err(io_error(&path))
    };
    let templates = read_int_pack(pack, &read("left-id.def")?, &read("right-id.def")?)?;
    write_int_files(&templates, dict_dir, hot, threads)
}
