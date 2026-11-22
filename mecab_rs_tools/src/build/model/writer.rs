use std::path::Path;

use crate::tool_error::{Position, build_error};
use mecab_rs::__private::dictionary_format::CHARSET_BYTES;
use mecab_rs::Error;

use super::fingerprint::fingerprint;
use super::{MODEL_COUNT_BYTES, MODEL_ENTRY_BYTES};
use crate::build::Charset;
use crate::build::text::{LINE_BUFFER_BYTES, Lines, atof, cstr, tokenize2};

fn charset_field(charset: &str) -> [u8; CHARSET_BYTES] {
    let mut field = [0u8; CHARSET_BYTES];
    let length = charset.len().min(CHARSET_BYTES - 1);
    field[..length].copy_from_slice(&charset.as_bytes()[..length]);
    field
}

struct ModelEntry {
    fingerprint: u64,
    weight: f64,
}

fn read_model_charset(lines: &mut Lines, text_model: &Path) -> Result<Vec<u8>, Error> {
    let mut model_charset = Vec::new();
    while let Some((number, line)) = lines.next_line()? {
        if line[0] == 0 {
            break;
        }
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let found = tokenize2(line, b":", 2);
        if found.len() != 2 {
            return Err(Position::new(text_model, number).build(format!("format error: {shown}")));
        }
        if cstr(line, found[0]) == b"charset" {
            model_charset = cstr(line, found[1]).get(1..).unwrap_or_default().to_vec();
        }
    }
    Ok(model_charset)
}

fn require_same_charset(
    text_model: &Path,
    dictionary_charset: &str,
    model_charset: &[u8],
) -> Result<(), Error> {
    if dictionary_charset.is_empty() {
        return Ok(());
    }
    let model = Charset::parse(&String::from_utf8_lossy(model_charset))
        .map_err(|reason| build_error(text_model, reason))?;
    let dictionary =
        Charset::parse(dictionary_charset).map_err(|reason| build_error(text_model, reason))?;
    if model != dictionary {
        return Err(build_error(
            text_model,
            format!(
                "dictionary charset and model charset are different. dictionary_charset={dictionary_charset} model_charset={}",
                String::from_utf8_lossy(model_charset)
            ),
        ));
    }
    Ok(())
}

fn read_entries(lines: &mut Lines, text_model: &Path) -> Result<Vec<ModelEntry>, Error> {
    let mut entries = Vec::new();
    while let Some((number, line)) = lines.next_line()? {
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let found = tokenize2(line, b"\t", 2);
        if found.len() != 2 {
            return Err(Position::new(text_model, number).build(format!("format error: {shown}")));
        }
        entries.push(ModelEntry {
            fingerprint: fingerprint(cstr(line, found[1])),
            weight: atof(cstr(line, found[0])),
        });
    }
    Ok(entries)
}

pub(crate) fn convert(
    text_model: &Path,
    dictionary_charset: &str,
    charset: &str,
) -> Result<Vec<u8>, Error> {
    let mut lines = Lines::open(text_model, Some(LINE_BUFFER_BYTES))?;
    let model_charset = read_model_charset(&mut lines, text_model)?;
    require_same_charset(text_model, dictionary_charset, &model_charset)?;
    let mut entries = read_entries(&mut lines, text_model)?;
    entries.sort_by(|a, b| {
        a.fingerprint.cmp(&b.fingerprint).then_with(|| {
            a.weight
                .partial_cmp(&b.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    let mut bytes =
        Vec::with_capacity(MODEL_COUNT_BYTES + CHARSET_BYTES + entries.len() * MODEL_ENTRY_BYTES);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&charset_field(charset));
    for entry in &entries {
        bytes.extend_from_slice(&entry.weight.to_le_bytes());
    }
    for entry in &entries {
        bytes.extend_from_slice(&entry.fingerprint.to_le_bytes());
    }
    Ok(bytes)
}
