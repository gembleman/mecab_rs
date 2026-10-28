use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use mecab_rs::__private::io_error;
use mecab_rs::__private::u32_at;
use mecab_rs::__private::{HEADER_SIZE, MAGIC_ID, TOKEN_SIZE};
use mecab_rs::__private::{write_file, write_file_with};
use mecab_rs::{Dictionary, Error, Tagger};

pub mod int_matrix;
mod runtime;
mod templates;

pub use mecab_rs::__private::{MAX_HOT_THREADS, default_threads};
pub use runtime::{RuntimeOptions, RuntimeReport, StatsSource, build_runtime};
pub use templates::{DEFAULT_HOT_CELLS, DEFAULT_HOT_LEFT, DEFAULT_HOT_RIGHT, compile_templates};

const CONTEXT_FILES: [&str; 2] = ["sys.dic", "unk.dic"];
const ID_DEF_FILES: [&str; 2] = ["left-id.def", "right-id.def"];
pub const UNIDIC_FEATURE_COUNT: usize = 29;

const MATRIX_FILE: &str = "matrix.bin";
const RIGHT_STATS: &str = "right.tsv";
const LEFT_STATS: &str = "left.tsv";
fn bad(path: &Path, reason: impl Into<String>) -> Error {
    Error::Rebuild {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

const WORD_BYTES: usize = 4;

fn word(bytes: &[u8], index: usize) -> u32 {
    u32_at(bytes, index * WORD_BYTES)
}

fn read_file(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(io_error(path))
}

fn copy_entries(source: &Path, target: &Path, only: Option<&[&str]>) -> Result<(), Error> {
    fs::create_dir_all(target).map_err(io_error(target))?;
    for entry in fs::read_dir(source).map_err(io_error(source))? {
        let entry = entry.map_err(io_error(source))?;
        let name = entry.file_name();
        if only.is_some_and(|wanted| !wanted.iter().any(|wanted| name == *wanted)) {
            continue;
        }
        let from = entry.path();
        let to = target.join(&name);
        if from.is_dir() {
            copy_entries(&from, &to, None)?;
        } else {
            fs::copy(&from, &to).map_err(io_error(&from))?;
        }
    }
    Ok(())
}

fn prepare_output(dict_dir: &Path, out_dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(out_dir).map_err(io_error(out_dir))?;
    let same = fs::canonicalize(dict_dir).map_err(io_error(dict_dir))?
        == fs::canonicalize(out_dir).map_err(io_error(out_dir))?;
    if same {
        return Err(bad(
            out_dir,
            "the output directory must differ from the source",
        ));
    }
    Ok(())
}

fn matrix_shape(dict_dir: &Path) -> Result<(usize, usize), Error> {
    let path = dict_dir.join(MATRIX_FILE);
    let mut header = [0u8; 4];
    File::open(&path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(io_error(&path))?;
    Ok((
        usize::from(u16::from_le_bytes([header[0], header[1]])),
        usize::from(u16::from_le_bytes([header[2], header[3]])),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextStats {
    pub right: Vec<u64>,
    pub left: Vec<u64>,
}

impl ContextStats {
    pub fn write_tsv(&self, directory: &Path) -> Result<(), Error> {
        fs::create_dir_all(directory).map_err(io_error(directory))?;
        for (name, counts) in [(RIGHT_STATS, &self.right), (LEFT_STATS, &self.left)] {
            write_file(&directory.join(name), |writer| {
                for (id, count) in counts.iter().enumerate() {
                    writeln!(writer, "{id}\t{count}")?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn read_tsv(directory: &Path) -> Result<Self, Error> {
        Ok(Self {
            right: read_counts(&directory.join(RIGHT_STATS))?,
            left: read_counts(&directory.join(LEFT_STATS))?,
        })
    }
}

fn read_counts(path: &Path) -> Result<Vec<u64>, Error> {
    let text = String::from_utf8(read_file(path)?).map_err(|_| bad(path, "not UTF-8"))?;
    let mut counts = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let parsed = line
            .split_once('\t')
            .and_then(|(id, count)| Some((id.parse::<usize>().ok()?, count.parse::<u64>().ok()?)));
        match parsed {
            Some((id, count)) if id == counts.len() => counts.push(count),
            _ => {
                return Err(bad(
                    path,
                    format!("line {}: expected `{}<TAB>count`", index + 1, counts.len()),
                ));
            }
        }
    }
    Ok(counts)
}

pub fn collect_stats(dict_dir: &Path, corpora: &[PathBuf]) -> Result<ContextStats, Error> {
    let (left_size, right_size) = matrix_shape(dict_dir)?;
    let runtime = std::env::temp_dir().join(format!(
        "mecab-rs-rebuild-{}-{}.mrd",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    let result = (|| {
        mecab_rs::packed::write_runtime(dict_dir, &runtime)?;
        let dictionary = Dictionary::open(&runtime)?;
        let mut tagger = Tagger::new(&dictionary);
        let mut stats = ContextStats {
            right: vec![0; left_size],
            left: vec![0; right_size],
        };
        for corpus in corpora {
            let bytes = read_file(corpus)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| bad(corpus, "not UTF-8"))?;
            for line in text.split_terminator('\n') {
                let sentence = line.strip_suffix('\r').unwrap_or(line);
                let view = tagger.parse_lattice(sentence, &[])?;
                for position in 0..=sentence.len() {
                    let ends = view.end_nodes(position);
                    let begins = view.begin_nodes(position);
                    if ends.len() == 0 || begins.len() == 0 {
                        continue;
                    }
                    let (end_count, begin_count) = (ends.len() as u64, begins.len() as u64);
                    for node in ends {
                        stats.right[usize::from(node.right_id())] += begin_count;
                    }
                    for node in begins {
                        stats.left[usize::from(node.left_id())] += end_count;
                    }
                }
            }
        }
        Ok(stats)
    })();
    let _ = fs::remove_file(&runtime);
    result
}

fn order_by_frequency(counts: &[u64]) -> Vec<usize> {
    let mut order: Vec<usize> = (1..counts.len()).collect();
    order.sort_by_key(|&id| (std::cmp::Reverse(counts[id]), id));
    if !counts.is_empty() {
        order.insert(0, 0);
    }
    order
}

fn inverse_of(order: &[usize]) -> Vec<usize> {
    let mut inverse = vec![0; order.len()];
    for (new_id, &old_id) in order.iter().enumerate() {
        inverse[old_id] = new_id;
    }
    inverse
}

fn patch_tokens(
    data: &mut [u8],
    path: &Path,
    new_lc: &[usize],
    new_rc: &[usize],
) -> Result<(), Error> {
    if data.len() < HEADER_SIZE {
        return Err(bad(path, "shorter than the 72-byte header"));
    }
    let (dsize, tsize) = (word(data, 6) as usize, word(data, 7) as usize);
    let start = HEADER_SIZE + dsize;
    if tsize % TOKEN_SIZE != 0 || start.checked_add(tsize).is_none_or(|end| end > data.len()) {
        return Err(bad(path, "the token section does not fit the file"));
    }
    for token in data[start..start + tsize]
        .as_chunks_mut::<TOKEN_SIZE>()
        .0
        .iter_mut()
    {
        for (field, map) in [(0, new_lc), (2, new_rc)] {
            let old = usize::from(u16::from_le_bytes([token[field], token[field + 1]]));
            let new = map
                .get(old)
                .ok_or_else(|| bad(path, format!("a token uses id {old}, outside the matrix")))?;
            token[field..field + 2].copy_from_slice(&(*new as u16).to_le_bytes());
        }
    }
    Ok(())
}

fn renumber_id_def(text: &str, order: &[usize], path: &Path) -> Result<String, Error> {
    let mut rows: Vec<&str> = text.split('\n').collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }
    if rows.len() != order.len() {
        return Err(bad(
            path,
            format!(
                "{} rows, but the matrix expects {}",
                rows.len(),
                order.len()
            ),
        ));
    }
    let mut attributes = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let (_, rest) = row
            .split_once(' ')
            .ok_or_else(|| bad(path, format!("row {} has no attributes", index + 1)))?;
        attributes.push(rest);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (new_id, &old_id) in order.iter().enumerate() {
        out.push_str(&format!("{new_id} {}\n", attributes[old_id]));
    }
    Ok(out)
}

fn write_permuted_matrix(
    writer: &mut impl Write,
    matrix: &[u8],
    path: &Path,
    order_lc: &[usize],
    order_rc: &[usize],
) -> Result<(), Error> {
    let (left_size, right_size) = (order_rc.len(), order_lc.len());
    if matrix.len() != 4 + 2 * left_size * right_size {
        return Err(bad(
            path,
            format!(
                "{} bytes, but a {left_size} x {right_size} matrix takes {}",
                matrix.len(),
                4 + 2 * left_size * right_size
            ),
        ));
    }
    let sink = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    writer.write_all(&matrix[..4]).map_err(sink)?;
    let mut row = vec![0u8; left_size * 2];
    for &old_row in order_lc {
        let from = &matrix[4 + old_row * left_size * 2..][..left_size * 2];
        for (cell, &old_column) in row.as_chunks_mut::<2>().0.iter_mut().zip(order_rc) {
            cell.copy_from_slice(&from[old_column * 2..][..2]);
        }
        writer.write_all(&row).map_err(sink)?;
    }
    Ok(())
}

pub(crate) struct Renumbering {
    pub(crate) left_size: usize,
    pub(crate) right_size: usize,
    order_lc: Vec<usize>,
    order_rc: Vec<usize>,
    new_lc: Vec<usize>,
    new_rc: Vec<usize>,
    pub(crate) id_defs: Vec<(&'static str, String)>,
}

impl Renumbering {
    pub(crate) fn plan(
        dict_dir: &Path,
        stats: &ContextStats,
        stats_dir: &Path,
    ) -> Result<Self, Error> {
        let (left_size, right_size) = matrix_shape(dict_dir)?;
        if stats.right.len() != left_size || stats.left.len() != right_size {
            return Err(bad(
                stats_dir,
                format!(
                    "right.tsv has {} ids and left.tsv {}, but the matrix is {left_size} x {right_size}",
                    stats.right.len(),
                    stats.left.len()
                ),
            ));
        }
        let order_rc = order_by_frequency(&stats.right);
        let order_lc = order_by_frequency(&stats.left);
        let (new_rc, new_lc) = (inverse_of(&order_rc), inverse_of(&order_lc));
        let mut id_defs = Vec::new();
        for (name, order) in ID_DEF_FILES.iter().zip([&order_lc, &order_rc]) {
            let source = dict_dir.join(name);
            let text =
                String::from_utf8(read_file(&source)?).map_err(|_| bad(&source, "not UTF-8"))?;
            id_defs.push((*name, renumber_id_def(&text, order, &source)?));
        }
        Ok(Self {
            left_size,
            right_size,
            order_lc,
            order_rc,
            new_lc,
            new_rc,
            id_defs,
        })
    }

    pub(crate) fn write_dictionaries(
        &self,
        dict_dir: &Path,
        out_dir: &Path,
        keep: Option<&Keep>,
    ) -> Result<Option<SlimReport>, Error> {
        let mut report = None;
        for name in CONTEXT_FILES {
            let source = dict_dir.join(name);
            let mut data = read_file(&source)?;
            patch_tokens(&mut data, &source, &self.new_lc, &self.new_rc)?;
            if let (Some(keep), "sys.dic") = (keep, name) {
                let (slimmed, slim_report) = slim_dictionary(&data, &source, keep)?;
                data = slimmed;
                report = Some(slim_report);
            }
            write_file(&out_dir.join(name), |writer| writer.write_all(&data))?;
        }
        Ok(report)
    }

    pub(crate) fn write_id_defs(&self, out_dir: &Path) -> Result<(), Error> {
        for (name, text) in &self.id_defs {
            write_file(&out_dir.join(name), |writer| {
                writer.write_all(text.as_bytes())
            })?;
        }
        Ok(())
    }

    pub(crate) fn write_matrix(&self, dict_dir: &Path, out_dir: &Path) -> Result<(), Error> {
        let matrix_path = dict_dir.join(MATRIX_FILE);
        let file = File::open(&matrix_path).map_err(io_error(&matrix_path))?;
        let matrix = unsafe { Mmap::map(&file) }.map_err(io_error(&matrix_path))?;
        write_file_with(&out_dir.join(MATRIX_FILE), |writer| {
            write_permuted_matrix(
                writer,
                &matrix,
                &matrix_path,
                &self.order_lc,
                &self.order_rc,
            )
        })
    }
}

fn split_fields(text: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, &byte) in text.iter().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                spans.push((start, index));
                start = index + 1;
            }
            _ => {}
        }
    }
    spans.push((start, text.len()));
    spans
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    Fields(Vec<usize>),
}

impl Keep {
    pub fn parse_fields(spec: &str) -> Result<Self, String> {
        let mut fields = Vec::new();
        for part in spec.split(',') {
            let number = |text: &str| {
                text.trim().parse::<usize>().map_err(|_| {
                    format!("`{part}` in the field set `{spec}` is not a number or range")
                })
            };
            match part.split_once('-') {
                Some((low, high)) => fields.extend(number(low)?..=number(high)?),
                None => fields.push(number(part)?),
            }
        }
        fields.sort_unstable();
        fields.dedup();
        Ok(Self::Fields(fields))
    }

    pub fn apply(&self, raw: &[u8]) -> Vec<u8> {
        let spans = split_fields(raw);
        match self {
            Self::Fields(kept) => {
                if spans.len() != UNIDIC_FEATURE_COUNT {
                    return raw.to_vec();
                }
                let mut out = Vec::with_capacity(raw.len());
                for (index, &(from, to)) in spans.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    if kept.binary_search(&index).is_ok() {
                        out.extend_from_slice(&raw[from..to]);
                    } else {
                        out.push(b'*');
                    }
                }
                out
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SlimReport {
    pub old_feature_size: usize,
    pub new_feature_size: usize,
    pub unique_features: usize,
    pub old_file_size: usize,
    pub new_file_size: usize,
}

fn slim_dictionary(data: &[u8], path: &Path, keep: &Keep) -> Result<(Vec<u8>, SlimReport), Error> {
    if data.len() < HEADER_SIZE {
        return Err(bad(path, "shorter than the 72-byte header"));
    }
    if word(data, 0) ^ MAGIC_ID != data.len() as u32 {
        return Err(bad(path, "magic does not match the file size"));
    }
    let (dsize, tsize, fsize) = (
        word(data, 6) as usize,
        word(data, 7) as usize,
        word(data, 8) as usize,
    );
    let token_start = HEADER_SIZE + dsize;
    let feature_start = token_start + tsize;
    if tsize % TOKEN_SIZE != 0 || feature_start + fsize != data.len() {
        return Err(bad(path, "the sections do not add up to the file size"));
    }
    let blob = &data[feature_start..];
    let mut tokens = data[token_start..feature_start].to_vec();
    let mut out_blob: Vec<u8> = Vec::new();
    let mut by_text: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut by_offset: HashMap<u32, u32> = HashMap::new();
    for token in tokens.as_chunks_mut::<TOKEN_SIZE>().0.iter_mut() {
        let offset = u32::from_le_bytes(token[8..12].try_into().unwrap());
        let place = match by_offset.get(&offset) {
            Some(&place) => place,
            None => {
                let tail = blob
                    .get(offset as usize..)
                    .ok_or_else(|| bad(path, "a feature offset is past the feature section"))?;
                let length = tail
                    .iter()
                    .position(|&byte| byte == 0)
                    .ok_or_else(|| bad(path, "a feature is not NUL-terminated"))?;
                let text = keep.apply(&tail[..length]);
                let place = match by_text.get(&text) {
                    Some(&place) => place,
                    None => {
                        let place = out_blob.len() as u32;
                        out_blob.extend_from_slice(&text);
                        out_blob.push(0);
                        by_text.insert(text, place);
                        place
                    }
                };
                by_offset.insert(offset, place);
                place
            }
        };
        token[8..12].copy_from_slice(&place.to_le_bytes());
    }
    let unique_features = by_text.len();
    while !out_blob.len().is_multiple_of(4) {
        out_blob.push(0);
    }
    let total = token_start + tsize + out_blob.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&data[..token_start]);
    out.extend_from_slice(&tokens);
    out.extend_from_slice(&out_blob);
    out[0..4].copy_from_slice(&(total as u32 ^ MAGIC_ID).to_le_bytes());
    out[32..36].copy_from_slice(&(out_blob.len() as u32).to_le_bytes());
    let report = SlimReport {
        old_feature_size: fsize,
        new_feature_size: out_blob.len(),
        unique_features,
        old_file_size: data.len(),
        new_file_size: total,
    };
    Ok((out, report))
}
