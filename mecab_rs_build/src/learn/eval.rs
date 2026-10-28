use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{failure, open_lines, surface_and_feature};
use crate::build::text::{BUF_SIZE, Lines, atoi, cstr, tokenize_csv, tokenize2};
use mecab_rs::Error;

struct Stream {
    lines: Lines,
    failed: bool,
}

type Sentence = Vec<Vec<Vec<u8>>>;

const LEVEL_TOKEN_LIMIT: usize = 512;

fn output_error(source: io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("output"),
        source,
    }
}

fn read_sentence(stream: &mut Stream, levels: &[i32]) -> Result<Option<Sentence>, Error> {
    if stream.failed {
        return Ok(None);
    }
    let path = stream.lines.path().to_path_buf();
    let mut words = Vec::new();
    loop {
        let Some(line) = stream.lines.next_line()? else {
            stream.failed = true;
            break;
        };
        if cstr(line, 0) == b"EOS" {
            break;
        }
        let text = cstr(line, 0);
        let Some((surface, feature)) = surface_and_feature(text) else {
            let shown = String::from_utf8_lossy(text);
            return Err(failure(&path, format!("format error: {shown}")));
        };
        let mut buffer = feature.to_vec();
        buffer.push(0);
        let starts = tokenize_csv(&mut buffer, BUF_SIZE - 1);
        let columns: Vec<&[u8]> = starts.iter().map(|&start| cstr(&buffer, start)).collect();
        let count = columns.len();
        let mut keys = Vec::with_capacity(levels.len());
        for &level in levels {
            let wanted = if level < 0 {
                count
            } else {
                usize::try_from(level).unwrap_or(0)
            };
            if wanted > count {
                return Err(failure(&path, format!(" out of range {level}")));
            }
            let mut key = surface.to_vec();
            for column in &columns[..wanted] {
                key.extend_from_slice(column);
                key.push(b'\t');
            }
            keys.push(key);
        }
        words.push(keys);
    }
    Ok(Some(words))
}

fn print_evaluation(
    out: &mut dyn Write,
    correct: usize,
    precision: usize,
    recall: usize,
) -> io::Result<()> {
    let p = if precision == 0 {
        0.0
    } else {
        100.0 * correct as f64 / precision as f64
    };
    let r = if recall == 0 {
        0.0
    } else {
        100.0 * correct as f64 / recall as f64
    };
    let f = if p + r == 0.0 {
        0.0
    } else {
        2.0 * p * r / (p + r)
    };
    writeln!(
        out,
        "{p:4.4}({}/{}) {r:4.4}({}/{}) {f:4.4}",
        correct as i32, precision as i32, correct as i32, recall as i32
    )
}

pub fn evaluate(
    system: &Path,
    answer: &Path,
    levels: &str,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let open = |path: &Path| {
        open_lines(path).map(|lines| Stream {
            lines,
            failed: false,
        })
    };
    let mut first = open(system)?;
    let mut second = open(answer)?;
    if levels.is_empty() {
        return Err(failure(system, "level_str is NULL"));
    }
    let mut buffer = levels.as_bytes().to_vec();
    buffer.push(0);
    let levels: Vec<i32> = tokenize2(&mut buffer, b"\t ", LEVEL_TOKEN_LIMIT)
        .into_iter()
        .map(|start| atoi(cstr(&buffer, start)))
        .collect();
    if levels.is_empty() {
        return Err(failure(system, "level_str is empty"));
    }
    let mut correct = vec![0usize; levels.len()];
    let mut precision = 0usize;
    let mut recall = 0usize;

    while let Some(r1) = read_sentence(&mut first, &levels)? {
        let Some(r2) = read_sentence(&mut second, &levels)? else {
            break;
        };
        let (mut i1, mut i2, mut p1, mut p2) = (0, 0, 0, 0);
        while i1 < r1.len() && i2 < r2.len() {
            if p1 == p2 {
                for (level, count) in correct.iter_mut().enumerate() {
                    if r1[i1][level] == r2[i2][level] {
                        *count += 1;
                    }
                }
                p1 += r1[i1][0].len();
                p2 += r2[i2][0].len();
                i1 += 1;
                i2 += 1;
                precision += 1;
                recall += 1;
            } else if p1 < p2 {
                p1 += r1[i1][0].len();
                i1 += 1;
                precision += 1;
            } else {
                p2 += r2[i2][0].len();
                i2 += 1;
                recall += 1;
            }
        }
        precision += r1.len() - i1;
        recall += r2.len() - i2;
    }

    writeln!(out, "              precision          recall         F").map_err(output_error)?;
    for (level, &count) in levels.iter().zip(&correct) {
        if *level == -1 {
            write!(out, "LEVEL ALL: ").map_err(output_error)?;
        } else {
            write!(out, "LEVEL {level}:    ").map_err(output_error)?;
        }
        print_evaluation(out, count, precision, recall).map_err(output_error)?;
    }
    Ok(())
}

pub fn test_sentences(inputs: &[PathBuf], out: &mut dyn Write) -> Result<(), Error> {
    let stdin = [PathBuf::from("-")];
    let inputs = if inputs.is_empty() { &stdin } else { inputs };
    let mut sentence: Vec<u8> = Vec::new();
    for input in inputs {
        let mut lines = if input.as_os_str() == "-" {
            Lines::stdin(input, Some(BUF_SIZE))
        } else {
            open_lines(input)?
        };
        while let Some(line) = lines.next_line()? {
            let text = cstr(line, 0);
            let end = text
                .iter()
                .position(|&byte| byte == b'\t' || byte == b' ')
                .unwrap_or(text.len());
            let surface = &text[..end];
            if surface == b"EOS" && !sentence.is_empty() {
                out.write_all(&sentence)
                    .and_then(|()| out.write_all(b"\n"))
                    .map_err(output_error)?;
                sentence.clear();
            } else {
                sentence.extend_from_slice(surface);
            }
        }
    }
    Ok(())
}
