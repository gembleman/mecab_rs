use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::model::{Model, Param};
use crate::Error;
use crate::build::text::key_value;
use crate::char_property::CharProperty;
use crate::char_table::CHAR_TABLE_FILE;
use crate::error::io_error;
use crate::hot_shape::HotShape;
use crate::int_templates::MappedIntTemplates;
use crate::system_dictionary::{
    DictionaryKind, HEADER_SIZE, SystemDictionary, TOKEN_SIZE, TokenRecord,
};
use crate::templates::float_header_sizes;

const ARRAY_BYTES_AT: usize = 24;
const TOKEN_BYTES_AT: usize = 28;
const MAX_ROOT_FANOUT: u32 = 256;

fn broken(path: &Path, reason: &'static str) -> Error {
    Error::Format {
        file: path.to_path_buf(),
        reason,
    }
}

fn dicrc_features(path: &Path) -> Result<(String, Option<String>), Error> {
    let bytes = fs::read(path).map_err(io_error(path))?;
    let mut values: HashMap<&[u8], &[u8]> = HashMap::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        if line.is_empty() || line[0] == b';' || line[0] == b'#' {
            continue;
        }
        let Some((key, value)) = key_value(line) else {
            return Err(broken(path, "a line has no '='"));
        };
        values.entry(key).or_insert(value);
    }
    let text = |value: &[u8]| {
        String::from_utf8(value.to_vec())
            .map_err(|_| broken(path, "bos-feature or unk-feature is not valid UTF-8"))
    };
    let unknown = values
        .get(b"unk-feature".as_slice())
        .filter(|value| !value.is_empty())
        .map(|value| text(value))
        .transpose()?;
    let bos = values
        .get(b"bos-feature".as_slice())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| broken(path, "bos-feature is missing"))?;
    Ok((text(bos)?, unknown))
}

struct RawKey {
    key: String,
    first_token: u32,
    count: u32,
}

fn read_keys(path: &Path) -> Result<Vec<RawKey>, Error> {
    let bytes = fs::read(path).map_err(io_error(path))?;
    let word = |at: usize| u32::from_le_bytes(*bytes[at..].first_chunk().expect("in range"));
    let end = HEADER_SIZE + word(ARRAY_BYTES_AT) as usize;
    let cells: Vec<[u32; 2]> = bytes[HEADER_SIZE..end]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|cell| {
            let [a, b, c, d, e, f, g, h] = *cell;
            [
                u32::from_le_bytes([a, b, c, d]),
                u32::from_le_bytes([e, f, g, h]),
            ]
        })
        .collect();
    let length = cells.len();
    let is_child = |index: usize, check: u32| {
        check != 0 && index > check as usize && index - check as usize <= MAX_ROOT_FANOUT as usize
    };
    let mut start = vec![0u32; length + 2];
    for (index, &[_, check]) in cells.iter().enumerate() {
        if is_child(index, check) {
            start[check as usize + 1] += 1;
        }
    }
    for at in 1..start.len() {
        start[at] += start[at - 1];
    }
    let mut fill = start.clone();
    let mut children = vec![0u32; start[length + 1] as usize];
    for (index, &[_, check]) in cells.iter().enumerate() {
        if is_child(index, check) {
            children[fill[check as usize] as usize] = index as u32;
            fill[check as usize] += 1;
        }
    }
    let Some(&[root, _]) = cells.first() else {
        return Ok(Vec::new());
    };
    let terminal_value = |base: u32| {
        cells
            .get(base as usize)
            .filter(|&&[value, check]| check == base && value.cast_signed() < 0)
            .map(|&[value, _]| !value)
    };
    let mut keys = Vec::new();
    let mut spelling: Vec<u8> = Vec::new();
    let mut stack: Vec<(u32, u32, usize)> = Vec::new();
    let visit = |base: u32, spelling: &[u8], keys: &mut Vec<RawKey>| -> Result<(), Error> {
        if let Some(value) = terminal_value(base) {
            let key = String::from_utf8(spelling.to_vec())
                .map_err(|_| Error::Unsupported("a sys.dic key that is not UTF-8"))?;
            keys.push(RawKey {
                key,
                first_token: value >> 8,
                count: value & 0xff,
            });
        }
        Ok(())
    };
    visit(root, &spelling, &mut keys)?;
    stack.push((root, start[root as usize], 0));
    while let Some(top) = stack.last_mut() {
        let (base, cursor, key_length) = *top;
        if base as usize >= length || cursor >= start[base as usize + 1] {
            stack.pop();
            continue;
        }
        top.1 += 1;
        let index = children[cursor as usize];
        spelling.truncate(key_length);
        spelling.push((index - base - 1) as u8);
        let child = cells[index as usize][0];
        if child.cast_signed() < 0 {
            continue;
        }
        visit(child, &spelling, &mut keys)?;
        stack.push((child, start[child as usize], spelling.len()));
    }
    let terminals = cells
        .iter()
        .enumerate()
        .filter(|&(index, &[value, check])| {
            index != 0 && check as usize == index && value.cast_signed() < 0
        })
        .count();
    if terminals != keys.len() {
        return Err(broken(
            path,
            "the double array has keys that are not reachable",
        ));
    }
    Ok(keys)
}

pub(super) fn read_dictionary(directory: &Path) -> Result<Model, Error> {
    let (bos_feature, unknown_feature) = dicrc_features(&directory.join("dicrc"))?;
    let system_path = directory.join("sys.dic");
    let system = SystemDictionary::open(&system_path, DictionaryKind::System)?;
    let unknown_path = directory.join("unk.dic");
    let unknown = SystemDictionary::open(&unknown_path, DictionaryKind::Unknown)?;
    let chars = CharProperty::open(&directory.join(CHAR_TABLE_FILE))?;
    let hot_path = directory.join("matrix.hot");
    let int_templates_path = directory.join("matrix.itp");
    let templates_path = directory.join("matrix.tpl");
    let integer_templates = hot_path.exists() && int_templates_path.exists();
    let hot = integer_templates || (hot_path.exists() && templates_path.exists());
    let matrix_path = if hot {
        hot_path
    } else {
        directory.join("matrix.bin")
    };
    if !hot && !matrix_path.exists() {
        return Err(Error::Unsupported(
            "a dictionary without matrix.bin or matrix.hot and matrix.tpl",
        ));
    }
    let matrix = fs::read(&matrix_path).map_err(io_error(&matrix_path))?;
    let templates_path = if integer_templates {
        int_templates_path
    } else {
        templates_path
    };
    let templates = if hot {
        fs::read(&templates_path).map_err(io_error(&templates_path))?
    } else {
        Vec::new()
    };
    let (left_size, right_size) = if hot {
        let (left_size, right_size, _) =
            HotShape::parse(&matrix).map_err(|reason| broken(&matrix_path, reason))?;
        (left_size, right_size)
    } else {
        let size = |at: usize| {
            matrix
                .get(at..at + 2)
                .map(|pair| usize::from(u16::from_le_bytes([pair[0], pair[1]])))
        };
        let (Some(left_size), Some(right_size)) = (size(0), size(2)) else {
            return Err(broken(&matrix_path, "the file is shorter than its header"));
        };
        if left_size == 0 || right_size == 0 || 4 + left_size * right_size * 2 != matrix.len() {
            return Err(broken(
                &matrix_path,
                "the file size does not match the header",
            ));
        }
        (left_size, right_size)
    };
    if hot && integer_templates {
        let mapped = MappedIntTemplates::open(&templates_path)?;
        if mapped.left_size() != left_size || mapped.right_size() != right_size {
            return Err(broken(
                &templates_path,
                "the integer templates do not match the hot matrix",
            ));
        }
    } else if hot && float_header_sizes(&templates) != Some((left_size, right_size)) {
        return Err(broken(
            &templates_path,
            "the templates are not valid or do not match the hot matrix",
        ));
    }
    let matches_matrix = |dictionary: &SystemDictionary| {
        dictionary.left_size() == left_size && dictionary.right_size() == right_size
    };
    if !matches_matrix(&system) || !matches_matrix(&unknown) {
        return Err(broken(
            &matrix_path,
            "the matrix size does not match sys.dic or unk.dic",
        ));
    }

    let raw_keys = read_keys(&system_path)?;
    let mut model = Model {
        matrix,
        templates,
        integer_templates,
        char_table: fs::read(directory.join(CHAR_TABLE_FILE))
            .map_err(io_error(&directory.join(CHAR_TABLE_FILE)))?,
        bos_feature,
        unknown_feature,
        ..Model::default()
    };
    let mut interner = Interner {
        params: HashMap::new(),
        features: HashMap::new(),
    };
    let mut position = 0u32;
    for raw in &raw_keys {
        if raw.first_token != position || raw.count == 0 || raw.count > 0xff {
            return Err(broken(
                &system_path,
                "the tokens are not in key order, or a key has no token",
            ));
        }
        position += raw.count;
    }
    if position as usize != system_token_count(&system_path)? {
        return Err(broken(&system_path, "some tokens belong to no key"));
    }
    for raw in raw_keys {
        model.counts.push(raw.count as u8);
        for index in raw.first_token..raw.first_token + raw.count {
            let (param, feature) =
                interner.intern(&mut model, &system, system.token(index as usize));
            model.token_params.push(param);
            model.token_features.push(feature);
        }
        model.keys.push(raw.key);
    }
    model.unknown_starts.push(0);
    for category in 0..chars.category_count() {
        let run = unknown
            .exact_match(chars.name(category))
            .ok_or_else(|| broken(&unknown_path, "a character category of char.uni is missing"))?;
        for index in run {
            let (param, feature) = interner.intern(&mut model, &unknown, unknown.token(index));
            model.unknown_params.push(param);
            model.unknown_features.push(feature);
        }
        model.unknown_starts.push(model.unknown_params.len() as u32);
    }
    Ok(model)
}

fn system_token_count(path: &Path) -> Result<usize, Error> {
    let mut header = [0u8; HEADER_SIZE];
    let mut file = fs::File::open(path).map_err(io_error(path))?;
    std::io::Read::read_exact(&mut file, &mut header).map_err(io_error(path))?;
    let word = |at: usize| u32::from_le_bytes(*header[at..].first_chunk().expect("in range"));
    Ok(word(TOKEN_BYTES_AT) as usize / TOKEN_SIZE)
}

struct Interner<'a> {
    params: HashMap<Param, u32>,
    features: HashMap<&'a str, u32>,
}

impl<'a> Interner<'a> {
    fn intern(
        &mut self,
        model: &mut Model,
        dictionary: &'a SystemDictionary,
        token: TokenRecord,
    ) -> (u32, u32) {
        let param = Param {
            left_id: token.left_id,
            right_id: token.right_id,
            pos_id: token.pos_id,
            word_cost: token.word_cost,
        };
        let next = self.params.len() as u32;
        let param = *self.params.entry(param).or_insert_with(|| {
            model.params.push(param);
            next
        });
        let text = dictionary.feature(token.feature);
        let next = self.features.len() as u32;
        let feature = *self.features.entry(text).or_insert_with(|| {
            model.features.extend_from_slice(text.as_bytes());
            model.features.push(0);
            next
        });
        (param, feature)
    }
}
