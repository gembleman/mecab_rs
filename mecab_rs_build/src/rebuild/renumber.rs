use std::fs::File;
use std::io::Write;
use std::path::Path;

use memmap2::Mmap;

use mecab_rs::__private::dictionary_format::{HEADER_SIZE, TOKEN_SIZE};
use mecab_rs::__private::files::{
    LEFT_ID_DEF_FILE, MATRIX_BIN_FILE, RIGHT_ID_DEF_FILE, SYS_DIC_FILE, UNK_DIC_FILE, io_error,
    write_file, write_file_with,
};
use mecab_rs::__private::in_range_bytes::u16_at;
use mecab_rs::Error;

use super::slim::{Keep, SlimReport, slim_dictionary};
use super::stats::{ContextStats, matrix_shape};
use super::{header_word, read_file};

const CONTEXT_FILES: [&str; 2] = [SYS_DIC_FILE, UNK_DIC_FILE];
const ID_DEF_FILES: [&str; 2] = [LEFT_ID_DEF_FILE, RIGHT_ID_DEF_FILE];

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
        return Err(Error::rebuild(path, "shorter than the 72-byte header"));
    }
    let (dsize, tsize) = (header_word(data, 6) as usize, header_word(data, 7) as usize);
    let start = HEADER_SIZE + dsize;
    if tsize % TOKEN_SIZE != 0 || start.checked_add(tsize).is_none_or(|end| end > data.len()) {
        return Err(Error::rebuild(
            path,
            "the token section does not fit the file",
        ));
    }
    for token in data[start..start + tsize]
        .as_chunks_mut::<TOKEN_SIZE>()
        .0
        .iter_mut()
    {
        for (field, map) in [(0, new_lc), (2, new_rc)] {
            let old = usize::from(u16_at(token, field));
            let new = map.get(old).ok_or_else(|| {
                Error::rebuild(path, format!("a token uses id {old}, outside the matrix"))
            })?;
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
        return Err(Error::rebuild(
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
            .ok_or_else(|| Error::rebuild(path, format!("row {} has no attributes", index + 1)))?;
        attributes.push(rest);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (new_id, &old_id) in order.iter().enumerate() {
        out.push_str(&new_id.to_string());
        out.push(' ');
        out.push_str(attributes[old_id]);
        out.push('\n');
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
        return Err(Error::rebuild(
            path,
            format!(
                "{} bytes, but a {left_size} x {right_size} matrix takes {}",
                matrix.len(),
                4 + 2 * left_size * right_size
            ),
        ));
    }
    let sink = io_error(path);
    writer.write_all(&matrix[..4]).map_err(&sink)?;
    let mut row = vec![0u8; left_size * 2];
    for &old_row in order_lc {
        let from = &matrix[4 + old_row * left_size * 2..][..left_size * 2];
        for (cell, &old_column) in row.as_chunks_mut::<2>().0.iter_mut().zip(order_rc) {
            cell.copy_from_slice(&from[old_column * 2..][..2]);
        }
        writer.write_all(&row).map_err(&sink)?;
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
            return Err(Error::rebuild(
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
            let text = String::from_utf8(read_file(&source)?)
                .map_err(|_| Error::rebuild(&source, "not UTF-8"))?;
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
            if let (Some(keep), SYS_DIC_FILE) = (keep, name) {
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
        let matrix_path = dict_dir.join(MATRIX_BIN_FILE);
        let file = File::open(&matrix_path).map_err(io_error(&matrix_path))?;
        let matrix = unsafe { Mmap::map(&file) }.map_err(io_error(&matrix_path))?;
        write_file_with(&out_dir.join(MATRIX_BIN_FILE), |writer| {
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
