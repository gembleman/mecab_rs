use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::tool_error::{caused_by, io_error, rebuild_error, rebuild_error_at};
use mecab_rs::__private::files::{MATRIX_BIN_FILE, write_file};
use mecab_rs::__private::in_range_bytes::u16_at;
use mecab_rs::IoAction;
use mecab_rs::{Constraints, Dictionary, Error, Tagger};

use super::read_file;

const RIGHT_STATS: &str = "right.tsv";
const LEFT_STATS: &str = "left.tsv";
const MATRIX_HEADER_PREFIX_BYTES: usize = 4;

pub(super) fn matrix_shape(dict_dir: &Path) -> Result<(usize, usize), Error> {
    let path = dict_dir.join(MATRIX_BIN_FILE);
    let mut header = [0u8; MATRIX_HEADER_PREFIX_BYTES];
    File::open(&path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(io_error(IoAction::Read, &path))?;
    Ok((
        usize::from(u16_at(&header, 0)),
        usize::from(u16_at(&header, 2)),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextStats {
    pub right: Vec<u64>,
    pub left: Vec<u64>,
}

impl ContextStats {
    pub fn write_tsv(&self, directory: &Path) -> Result<(), Error> {
        fs::create_dir_all(directory).map_err(io_error(IoAction::Create, directory))?;
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

    fn count_sentence(
        &mut self,
        tagger: &mut Tagger<&Dictionary>,
        sentence: &str,
    ) -> Result<(), Error> {
        let view = tagger.parse_lattice(&Constraints::new(sentence))?;
        for position in 0..=sentence.len() {
            let ends = view.end_nodes(position);
            let begins = view.begin_nodes(position);
            if ends.len() == 0 || begins.len() == 0 {
                continue;
            }
            let (end_count, begin_count) = (ends.len() as u64, begins.len() as u64);
            for node in ends {
                self.right[usize::from(node.right_id())] += begin_count;
            }
            for node in begins {
                self.left[usize::from(node.left_id())] += end_count;
            }
        }
        Ok(())
    }

    fn count_corpus(
        &mut self,
        tagger: &mut Tagger<&Dictionary>,
        corpus: &Path,
    ) -> Result<(), Error> {
        let bytes = read_file(corpus)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| rebuild_error(corpus, "not UTF-8"))?;
        for (index, line) in text.split_terminator('\n').enumerate() {
            let sentence = line.strip_suffix('\r').unwrap_or(line);
            self.count_sentence(tagger, sentence).map_err(|error| {
                caused_by(
                    rebuild_error_at(corpus, index + 1, "cannot analyse the sentence"),
                    error,
                )
            })?;
        }
        Ok(())
    }
}

fn read_counts(path: &Path) -> Result<Vec<u64>, Error> {
    let text = String::from_utf8(read_file(path)?).map_err(|_| rebuild_error(path, "not UTF-8"))?;
    let mut counts = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let parsed = line
            .split_once('\t')
            .and_then(|(id, count)| Some((id.parse::<usize>().ok()?, count.parse::<u64>().ok()?)));
        match parsed {
            Some((id, count)) if id == counts.len() => counts.push(count),
            _ => {
                return Err(rebuild_error_at(
                    path,
                    index + 1,
                    format!("expected `{}<TAB>count`", counts.len()),
                ));
            }
        }
    }
    Ok(counts)
}

fn temporary_runtime_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "mecab-rs-rebuild-{}-{}.mrd",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ))
}

fn count_with_runtime(
    dict_dir: &Path,
    runtime: &Path,
    corpora: &[PathBuf],
    sizes: (usize, usize),
) -> Result<ContextStats, Error> {
    mecab_rs::packed::write_runtime(dict_dir, runtime)?;
    let dictionary = Dictionary::open(runtime)?;
    let mut tagger = Tagger::new(&dictionary);
    let (left_size, right_size) = sizes;
    let mut stats = ContextStats {
        right: vec![0; left_size],
        left: vec![0; right_size],
    };
    for corpus in corpora {
        stats.count_corpus(&mut tagger, corpus)?;
    }
    Ok(stats)
}

pub fn collect_stats(dict_dir: &Path, corpora: &[PathBuf]) -> Result<ContextStats, Error> {
    let sizes = matrix_shape(dict_dir)?;
    let runtime = temporary_runtime_path();
    let result = count_with_runtime(dict_dir, &runtime, corpora, sizes);
    let _ = fs::remove_file(&runtime);
    result
}
