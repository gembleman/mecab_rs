
use std::path::Path;

use super::Charset;
use super::text::{BUF_SIZE, Lines, atof, cstr, failure, tokenize_csv, tokenize2};
use crate::error::Error;

const BUFFER_SIZE: usize = 2048;
const COLUMN_LIMIT: usize = 64;
const CHARSET_BYTES: usize = 32;
const FINGERPRINT_SEED: u32 = 0xfd14_deff;

fn rotl(value: u32, shift: u32) -> u32 {
    value.rotate_left(shift)
}

fn fmix(mut hash: u32) -> u32 {
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^ (hash >> 16)
}

pub(crate) fn fingerprint(key: &[u8]) -> u64 {
    const C1: u32 = 0x239b_961b;
    const C2: u32 = 0xab0e_9789;
    const C3: u32 = 0x38b3_4ae5;
    const C4: u32 = 0xa1e3_8b93;
    let (mut h1, mut h2, mut h3, mut h4) = (
        FINGERPRINT_SEED,
        FINGERPRINT_SEED,
        FINGERPRINT_SEED,
        FINGERPRINT_SEED,
    );
    let (blocks, tail) = key.as_chunks::<16>();
    for block in blocks {
        let word =
            |index: usize| u32::from_le_bytes(block[index * 4..index * 4 + 4].try_into().unwrap());
        let (mut k1, mut k2, mut k3, mut k4) = (word(0), word(1), word(2), word(3));
        k1 = rotl(k1.wrapping_mul(C1), 15).wrapping_mul(C2);
        h1 ^= k1;
        h1 = rotl(h1, 19)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x561c_cd1b);
        k2 = rotl(k2.wrapping_mul(C2), 16).wrapping_mul(C3);
        h2 ^= k2;
        h2 = rotl(h2, 17)
            .wrapping_add(h3)
            .wrapping_mul(5)
            .wrapping_add(0x0bca_a747);
        k3 = rotl(k3.wrapping_mul(C3), 17).wrapping_mul(C4);
        h3 ^= k3;
        h3 = rotl(h3, 15)
            .wrapping_add(h4)
            .wrapping_mul(5)
            .wrapping_add(0x96cd_1c35);
        k4 = rotl(k4.wrapping_mul(C4), 18).wrapping_mul(C1);
        h4 ^= k4;
        h4 = rotl(h4, 13)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x32ac_3b17);
    }
    let mut ks = [0u32; 4];
    for (index, &byte) in tail.iter().enumerate() {
        ks[index / 4] ^= u32::from(byte) << (8 * (index % 4));
    }
    if tail.len() > 12 {
        h4 ^= rotl(ks[3].wrapping_mul(C4), 18).wrapping_mul(C1);
    }
    if tail.len() > 8 {
        h3 ^= rotl(ks[2].wrapping_mul(C3), 17).wrapping_mul(C4);
    }
    if tail.len() > 4 {
        h2 ^= rotl(ks[1].wrapping_mul(C2), 16).wrapping_mul(C3);
    }
    if !tail.is_empty() {
        h1 ^= rotl(ks[0].wrapping_mul(C1), 15).wrapping_mul(C2);
    }
    let length = key.len() as u32;
    h1 ^= length;
    h2 ^= length;
    h3 ^= length;
    h4 ^= length;
    h1 = h1.wrapping_add(h2).wrapping_add(h3).wrapping_add(h4);
    h2 = h2.wrapping_add(h1);
    h3 = h3.wrapping_add(h1);
    h4 = h4.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h3 = fmix(h3);
    h4 = fmix(h4);
    h1 = h1.wrapping_add(h2).wrapping_add(h3).wrapping_add(h4);
    h2 = h2.wrapping_add(h1);
    u64::from(h1) | u64::from(h2) << 32
}

fn charset_field(charset: &str) -> [u8; CHARSET_BYTES] {
    let mut field = [0u8; CHARSET_BYTES];
    let length = charset.len().min(CHARSET_BYTES - 1);
    field[..length].copy_from_slice(&charset.as_bytes()[..length]);
    field
}

pub(crate) fn convert(
    text_model: &Path,
    dictionary_charset: &str,
    charset: &str,
) -> Result<Vec<u8>, Error> {
    let mut lines = Lines::open(text_model, Some(BUF_SIZE))?;
    let mut model_charset = Vec::new();
    while let Some(line) = lines.next_line()? {
        if line[0] == 0 {
            break;
        }
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let found = tokenize2(line, b":", 2);
        if found.len() != 2 {
            return Err(failure(text_model, format!("format error: {shown}")));
        }
        if cstr(line, found[0]) == b"charset" {
            model_charset = cstr(line, found[1]).get(1..).unwrap_or_default().to_vec();
        }
    }
    if !dictionary_charset.is_empty() {
        let model = Charset::decode(&String::from_utf8_lossy(&model_charset));
        if model != Charset::decode(dictionary_charset) {
            return Err(failure(
                text_model,
                format!(
                    "dictionary charset and model charset are different. dictionary_charset={dictionary_charset} model_charset={}",
                    String::from_utf8_lossy(&model_charset)
                ),
            ));
        }
    }

    let mut entries: Vec<(u64, f64)> = Vec::new();
    while let Some(line) = lines.next_line()? {
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let found = tokenize2(line, b"\t", 2);
        if found.len() != 2 {
            return Err(failure(text_model, format!("format error: {shown}")));
        }
        entries.push((
            fingerprint(cstr(line, found[1])),
            atof(cstr(line, found[0])),
        ));
    }
    entries.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut bytes = Vec::with_capacity(4 + CHARSET_BYTES + entries.len() * 16);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&charset_field(charset));
    for &(_, weight) in &entries {
        bytes.extend_from_slice(&weight.to_le_bytes());
    }
    for &(key, _) in &entries {
        bytes.extend_from_slice(&key.to_le_bytes());
    }
    Ok(bytes)
}

pub(crate) struct Templates {
    pub(crate) unigram: Vec<Vec<u8>>,
    pub(crate) bigram: Vec<Vec<u8>>,
}

impl Templates {
    pub(crate) fn open(feature_def: &Path) -> Result<Self, Error> {
        let mut lines = Lines::open(feature_def, Some(BUF_SIZE))?;
        let mut templates = Self {
            unigram: Vec::new(),
            bigram: Vec::new(),
        };
        while let Some(line) = lines.next_line()? {
            if line[0] == 0 || line[0] == b'#' || line[0] == b' ' {
                continue;
            }
            let found = tokenize2(line, b"\t ", 2);
            if found.len() != 2 {
                return Err(failure(feature_def, "format error"));
            }
            match cstr(line, found[0]) {
                b"UNIGRAM" => templates.unigram.push(cstr(line, found[1]).to_vec()),
                b"BIGRAM" => templates.bigram.push(cstr(line, found[1]).to_vec()),
                _ => return Err(failure(feature_def, "format error")),
            }
        }
        Ok(templates)
    }
}

pub(crate) struct CostModel {
    weights: Vec<f64>,
    keys: Vec<u64>,
    templates: Templates,
}

fn column<'a>(
    templ: &[u8],
    at: &mut usize,
    columns: &[&'a [u8]],
    path: &Path,
) -> Result<Option<&'a [u8]>, Error> {
    *at += 1;
    let optional = templ.get(*at) == Some(&b'?');
    if optional {
        *at += 1;
    }
    if templ.get(*at) != Some(&b'[') {
        return Err(failure(path, "getIndex(): unmatched '['"));
    }
    *at += 1;
    let mut number = 0usize;
    loop {
        match templ.get(*at).copied().unwrap_or(0) {
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
            _ => return Err(failure(path, "unmatched '['")),
        }
        *at += 1;
    }
}

fn escaped(byte: u8) -> u8 {
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

fn csv_columns(feature: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut buffer = feature[..feature.len().min(BUFFER_SIZE - 1)].to_vec();
    buffer.push(0);
    let found = tokenize_csv(&mut buffer, COLUMN_LIMIT);
    (buffer, found)
}

fn up_to_nul(key: &[u8]) -> &[u8] {
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
    let (buffer, found) = csv_columns(ufeature);
    let columns: Vec<&[u8]> = found.iter().map(|&start| cstr(&buffer, start)).collect();
    let mut key = Vec::new();
    'templates: for templ in templates {
        key.clear();
        let mut at = 0;
        while at < templ.len() {
            match templ[at] {
                b'\\' => {
                    at += 1;
                    key.push(escaped(templ.get(at).copied().unwrap_or(0)));
                }
                b'%' => {
                    at += 1;
                    match templ.get(at).copied().unwrap_or(0) {
                        b'F' => match column(templ, &mut at, &columns, feature_def)? {
                            Some(text) => key.extend_from_slice(text),
                            None => continue 'templates,
                        },
                        b't' => key.extend_from_slice(char_type.to_string().as_bytes()),
                        b'u' => key.extend_from_slice(ufeature),
                        b'w' => key.extend_from_slice(surface),
                        other => {
                            return Err(failure(
                                feature_def,
                                format!("unknown meta char: {}", char::from(other)),
                            ));
                        }
                    }
                }
                other => key.push(other),
            }
            at += 1;
        }
        add(up_to_nul(&key));
    }
    Ok(())
}

pub(crate) fn bigram_keys(
    templates: &[Vec<u8>],
    rfeature: &[u8],
    lfeature: &[u8],
    feature_def: &Path,
    mut add: impl FnMut(&[u8]),
) -> Result<(), Error> {
    let (left_buffer, left_found) = csv_columns(rfeature);
    let (right_buffer, right_found) = csv_columns(lfeature);
    let left: Vec<&[u8]> = left_found
        .iter()
        .map(|&start| cstr(&left_buffer, start))
        .collect();
    let right: Vec<&[u8]> = right_found
        .iter()
        .map(|&start| cstr(&right_buffer, start))
        .collect();
    let mut key = Vec::new();
    'templates: for templ in templates {
        key.clear();
        let mut at = 0;
        while at < templ.len() {
            match templ[at] {
                b'\\' => {
                    at += 1;
                    key.push(escaped(templ.get(at).copied().unwrap_or(0)));
                }
                b'%' => {
                    at += 1;
                    match templ.get(at).copied().unwrap_or(0) {
                        b'L' => match column(templ, &mut at, &left, feature_def)? {
                            Some(text) => key.extend_from_slice(text),
                            None => continue 'templates,
                        },
                        b'R' => match column(templ, &mut at, &right, feature_def)? {
                            Some(text) => key.extend_from_slice(text),
                            None => continue 'templates,
                        },
                        b'l' => key.extend_from_slice(lfeature),
                        b'r' => key.extend_from_slice(rfeature),
                        other => {
                            return Err(failure(
                                feature_def,
                                format!("unknown meta char: {}", char::from(other)),
                            ));
                        }
                    }
                }
                other => key.push(other),
            }
            at += 1;
        }
        add(up_to_nul(&key));
    }
    Ok(())
}

impl CostModel {
    pub(crate) fn open(
        model: &Path,
        dic_dir: &Path,
        dictionary_charset: &str,
        charset: &str,
    ) -> Result<Self, Error> {
        let bytes = std::fs::read(model).map_err(|source| Error::Io {
            path: model.to_path_buf(),
            source,
        })?;
        let words = bytes
            .first_chunk::<4>()
            .map(|count| u32::from_le_bytes(*count) as usize);
        let binary = words.is_some_and(|count| count * 16 + 4 + CHARSET_BYTES == bytes.len());
        let bytes = if binary {
            let name = cstr(&bytes[4..4 + CHARSET_BYTES], 0);
            if Charset::decode(&String::from_utf8_lossy(name)) != Charset::decode(charset) {
                return Err(failure(
                    model,
                    format!(
                        "model charset and dictionary charset are different. model_charset={} dictionary_charset={charset}",
                        String::from_utf8_lossy(name)
                    ),
                ));
            }
            bytes
        } else {
            convert(model, dictionary_charset, charset)?
        };
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let body = &bytes[4 + CHARSET_BYTES..];
        let weights = body[..count * 8]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| f64::from_le_bytes(*word))
            .collect();
        let keys = body[count * 8..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect();

        let templates = Templates::open(&dic_dir.join("feature.def"))?;
        Ok(Self {
            weights,
            keys,
            templates,
        })
    }

    fn id(&self, key: &[u8]) -> Option<usize> {
        let print = fingerprint(key);
        let at = self.keys.partition_point(|&candidate| candidate < print);
        (self.keys.get(at) == Some(&print)).then_some(at)
    }

    pub(crate) fn cost(
        &self,
        ufeature: &[u8],
        char_type: u8,
        feature_def: &Path,
    ) -> Result<f64, Error> {
        let mut total = 0.0;
        unigram_keys(
            &self.templates.unigram,
            ufeature,
            char_type,
            b"",
            feature_def,
            |key| {
                if let Some(id) = self.id(key) {
                    total += self.weights[id];
                }
            },
        )?;
        Ok(total)
    }

    pub(crate) fn connection_cost(
        &self,
        rfeature: &[u8],
        lfeature: &[u8],
        feature_def: &Path,
    ) -> Result<f64, Error> {
        let mut total = 0.0;
        bigram_keys(
            &self.templates.bigram,
            rfeature,
            lfeature,
            feature_def,
            |key| {
                if let Some(id) = self.id(key) {
                    total += self.weights[id];
                }
            },
        )?;
        Ok(total)
    }
}
