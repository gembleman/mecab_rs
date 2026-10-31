use std::cmp::Ordering;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{open_lines, surface_and_feature};
use crate::build::text::{LINE_BUFFER_BYTES, Lines, atoi, cstr, tokenize_csv, tokenize2};
use crate::tool_error::{Position, learn_error, output_error};
use mecab_rs::Error;

struct Stream {
    lines: Lines,
    failed: bool,
}

type Sentence = Vec<Vec<Vec<u8>>>;

const LEVEL_TOKEN_LIMIT: usize = 512;

fn read_sentence(stream: &mut Stream, levels: &[i32]) -> Result<Option<Sentence>, Error> {
    if stream.failed {
        return Ok(None);
    }
    let path = stream.lines.path().to_path_buf();
    let mut words = Vec::new();
    loop {
        let Some((number, line)) = stream.lines.next_line()? else {
            stream.failed = true;
            break;
        };
        if cstr(line, 0) == b"EOS" {
            break;
        }
        let at = Position::new(&path, number);
        let text = cstr(line, 0);
        let Some((surface, feature)) = surface_and_feature(text) else {
            let shown = String::from_utf8_lossy(text);
            return Err(at.learn(format!("format error: {shown}")));
        };
        let mut buffer = feature.to_vec();
        buffer.push(0);
        let starts = tokenize_csv(&mut buffer, LINE_BUFFER_BYTES - 1);
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
                return Err(at.learn(format!(" out of range {level}")));
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

fn parse_levels(levels: &str, system: &Path) -> Result<Vec<i32>, Error> {
    if levels.is_empty() {
        return Err(learn_error(system, "level_str is NULL"));
    }
    let mut buffer = levels.as_bytes().to_vec();
    buffer.push(0);
    let levels: Vec<i32> = tokenize2(&mut buffer, b"\t ", LEVEL_TOKEN_LIMIT)
        .into_iter()
        .map(|start| atoi(cstr(&buffer, start)))
        .collect();
    if levels.is_empty() {
        return Err(learn_error(system, "level_str is empty"));
    }
    Ok(levels)
}

struct Scores {
    correct: Vec<usize>,
    precision: usize,
    recall: usize,
}

impl Scores {
    fn new(level_count: usize) -> Self {
        Self {
            correct: vec![0; level_count],
            precision: 0,
            recall: 0,
        }
    }

    fn add_pair(&mut self, system: &Sentence, answer: &Sentence) {
        let (mut i1, mut i2, mut p1, mut p2) = (0, 0, 0, 0);
        while i1 < system.len() && i2 < answer.len() {
            match p1.cmp(&p2) {
                Ordering::Equal => {
                    for (level, count) in self.correct.iter_mut().enumerate() {
                        if system[i1][level] == answer[i2][level] {
                            *count += 1;
                        }
                    }
                    p1 += system[i1][0].len();
                    p2 += answer[i2][0].len();
                    i1 += 1;
                    i2 += 1;
                    self.precision += 1;
                    self.recall += 1;
                }
                Ordering::Less => {
                    p1 += system[i1][0].len();
                    i1 += 1;
                    self.precision += 1;
                }
                Ordering::Greater => {
                    p2 += answer[i2][0].len();
                    i2 += 1;
                    self.recall += 1;
                }
            }
        }
        self.precision += system.len() - i1;
        self.recall += answer.len() - i2;
    }

    fn print(&self, levels: &[i32], out: &mut dyn Write) -> Result<(), Error> {
        writeln!(out, "              precision          recall         F").map_err(output_error)?;
        for (level, &count) in levels.iter().zip(&self.correct) {
            if *level == -1 {
                write!(out, "LEVEL ALL: ").map_err(output_error)?;
            } else {
                write!(out, "LEVEL {level}:    ").map_err(output_error)?;
            }
            print_evaluation(out, count, self.precision, self.recall).map_err(output_error)?;
        }
        Ok(())
    }
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
    let levels = parse_levels(levels, system)?;
    let mut scores = Scores::new(levels.len());
    while let Some(r1) = read_sentence(&mut first, &levels)? {
        let Some(r2) = read_sentence(&mut second, &levels)? else {
            break;
        };
        scores.add_pair(&r1, &r2);
    }
    scores.print(&levels, out)
}

pub fn test_sentences(inputs: &[PathBuf], out: &mut dyn Write) -> Result<(), Error> {
    let stdin = [PathBuf::from("-")];
    let inputs = if inputs.is_empty() { &stdin } else { inputs };
    let mut sentence: Vec<u8> = Vec::new();
    for input in inputs {
        let mut lines = if input.as_os_str() == "-" {
            Lines::stdin(input, Some(LINE_BUFFER_BYTES))
        } else {
            open_lines(input)?
        };
        while let Some((_, line)) = lines.next_line()? {
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
