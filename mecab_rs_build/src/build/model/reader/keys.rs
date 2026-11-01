use std::path::Path;

use crate::build::text::{cstr, tokenize_csv};
use crate::tool_error::build_error;
use mecab_rs::Error;

const FEATURE_BUFFER_BYTES: usize = 2048;
const MAX_FEATURE_COLUMNS: usize = 64;

pub(super) fn template_column<'a>(
    template: &[u8],
    cursor: &mut usize,
    columns: &[&'a [u8]],
    path: &Path,
) -> Result<Option<&'a [u8]>, Error> {
    *cursor += 1;
    let optional = template.get(*cursor) == Some(&b'?');
    if optional {
        *cursor += 1;
    }
    if template.get(*cursor) != Some(&b'[') {
        return Err(build_error(path, "getIndex(): unmatched '['"));
    }
    *cursor += 1;
    let mut number = 0usize;
    loop {
        match template.get(*cursor).copied().unwrap_or(0) {
            digit @ b'0'..=b'9' => {
                number = number
                    .wrapping_mul(10)
                    .wrapping_add(usize::from(digit - b'0'));
            }
            b']' => {
                if number >= columns.len() {
                    return Ok(None);
                }
                let found = columns[number];
                if optional && (found == b"*" || found.is_empty()) {
                    return Ok(None);
                }
                return Ok(Some(found));
            }
            _ => return Err(build_error(path, "unmatched '['")),
        }
        *cursor += 1;
    }
}

pub(super) fn escape_value(byte: u8) -> u8 {
    match byte {
        b'a' => 0x07,
        b'b' => 0x08,
        b't' => b'\t',
        b'n' => b'\n',
        b'v' => 0x0b,
        b'f' => 0x0c,
        b'r' => b'\r',
        b's' => b' ',
        b'\\' => b'\\',
        _ => 0,
    }
}

pub(super) struct CsvColumns {
    pub(super) buffer: Vec<u8>,
    pub(super) starts: Vec<usize>,
}

impl CsvColumns {
    pub(super) fn cut(feature: &[u8]) -> Self {
        let mut buffer = feature[..feature.len().min(FEATURE_BUFFER_BYTES - 1)].to_vec();
        buffer.push(0);
        let starts = tokenize_csv(&mut buffer, MAX_FEATURE_COLUMNS);
        Self { buffer, starts }
    }

    pub(super) fn columns(&self) -> Vec<&[u8]> {
        self.starts
            .iter()
            .map(|&start| cstr(&self.buffer, start))
            .collect()
    }
}

pub(super) fn up_to_nul(key: &[u8]) -> &[u8] {
    &key[..key.iter().position(|&byte| byte == 0).unwrap_or(key.len())]
}

pub(crate) fn unigram_keys(
    templates: &[Vec<u8>],
    ufeature: &[u8],
    char_type: u8,
    surface: &[u8],
    feature_def: &Path,
    mut add: impl FnMut(&[u8]),
) -> Result<(), Error> {
    let cut = CsvColumns::cut(ufeature);
    let columns = cut.columns();
    let mut key = Vec::new();
    'templates: for template in templates {
        key.clear();
        let mut cursor = 0;
        while cursor < template.len() {
            match template[cursor] {
                b'\\' => {
                    cursor += 1;
                    key.push(escape_value(template.get(cursor).copied().unwrap_or(0)));
                }
                b'%' => {
                    cursor += 1;
                    match template.get(cursor).copied().unwrap_or(0) {
                        b'F' => {
                            match template_column(template, &mut cursor, &columns, feature_def)? {
                                Some(text) => key.extend_from_slice(text),
                                None => continue 'templates,
                            }
                        }
                        b't' => key.extend_from_slice(char_type.to_string().as_bytes()),
                        b'u' => key.extend_from_slice(ufeature),
                        b'w' => key.extend_from_slice(surface),
                        other => {
                            return Err(build_error(
                                feature_def,
                                format!("unknown meta char: {}", char::from(other)),
                            ));
                        }
                    }
                }
                other => key.push(other),
            }
            cursor += 1;
        }
        add(up_to_nul(&key));
    }
    Ok(())
}
