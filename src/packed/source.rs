use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::model::{Model, Param};
use crate::Error;
use crate::char_property::CharProperty;
use crate::dict_files::{
    CHAR_TABLE_FILE, DICRC_FILE, MATRIX_BIN_FILE, MATRIX_HOT_FILE, MATRIX_ITP_FILE, SYS_DIC_FILE,
    UNK_DIC_FILE,
};
use crate::error::io_error;
use crate::hot_shape::HotShape;
use crate::int_templates::MappedIntTemplates;
use crate::system_dictionary::{
    ARRAY_BYTES_AT, DictionaryKind, HEADER_SIZE, SystemDictionary, TOKEN_BYTES_AT, TOKEN_SIZE,
    TokenRecord, VALUE_SIZE_BITS,
};
use crate::wire::{u16_at, u32_at};

const MAX_ROOT_FANOUT: u32 = 256;
const MAX_KEY_TOKENS: u32 = (1 << VALUE_SIZE_BITS) - 1;

fn key_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let equals = line.iter().position(|&byte| byte == b'=')?;
    let key_end = line[..equals]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |offset| offset + 1);
    let value_start = line[equals + 1..]
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map_or(line.len(), |offset| equals + 1 + offset);
    Some((&line[..key_end], &line[value_start..]))
}

fn dicrc_features(path: &Path) -> Result<(String, Option<String>), Error> {
    let bytes = fs::read(path).map_err(io_error(path))?;
    let mut values: HashMap<&[u8], &[u8]> = HashMap::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        if line.is_empty() || line[0] == b';' || line[0] == b'#' {
            continue;
        }
        let Some((key, value)) = key_value(line) else {
            return Err(Error::format(path, "a line has no '='"));
        };
        values.entry(key).or_insert(value);
    }
    let text = |value: &[u8]| {
        String::from_utf8(value.to_vec())
            .map_err(|_| Error::format(path, "bos-feature or unk-feature is not valid UTF-8"))
    };
    let unknown = values
        .get(b"unk-feature".as_slice())
        .filter(|value| !value.is_empty())
        .map(|value| text(value))
        .transpose()?;
    let bos = values
        .get(b"bos-feature".as_slice())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::format(path, "bos-feature is missing"))?;
    Ok((text(bos)?, unknown))
}

struct RawKey {
    key: String,
    first_token: u32,
    count: u32,
}

struct ChildIndex {
    start: Vec<u32>,
    children: Vec<u32>,
}

fn is_child(index: usize, check: u32) -> bool {
    check != 0 && index > check as usize && index - check as usize <= MAX_ROOT_FANOUT as usize
}

fn child_index(cells: &[[u32; 2]]) -> ChildIndex {
    let mut start = vec![0u32; cells.len() + 2];
    for (index, &[_, check]) in cells.iter().enumerate() {
        if is_child(index, check) {
            start[check as usize + 1] += 1;
        }
    }
    for at in 1..start.len() {
        start[at] += start[at - 1];
    }
    let mut fill = start.clone();
    let mut children = vec![0u32; start[cells.len() + 1] as usize];
    for (index, &[_, check]) in cells.iter().enumerate() {
        if is_child(index, check) {
            children[fill[check as usize] as usize] = index as u32;
            fill[check as usize] += 1;
        }
    }
    ChildIndex { start, children }
}

fn terminal_value(cells: &[[u32; 2]], base: u32) -> Option<u32> {
    cells
        .get(base as usize)
        .filter(|&&[value, check]| check == base && value.cast_signed() < 0)
        .map(|&[value, _]| !value)
}

fn visit_terminal(
    cells: &[[u32; 2]],
    base: u32,
    spelling: &[u8],
    keys: &mut Vec<RawKey>,
) -> Result<(), Error> {
    if let Some(value) = terminal_value(cells, base) {
        let key = String::from_utf8(spelling.to_vec())
            .map_err(|_| Error::Unsupported("a sys.dic key that is not UTF-8"))?;
        keys.push(RawKey {
            key,
            first_token: value >> VALUE_SIZE_BITS,
            count: value & MAX_KEY_TOKENS,
        });
    }
    Ok(())
}

fn walk_trie(cells: &[[u32; 2]], index: &ChildIndex, root: u32) -> Result<Vec<RawKey>, Error> {
    let ChildIndex { start, children } = index;
    let mut keys = Vec::new();
    let mut spelling: Vec<u8> = Vec::new();
    let mut stack: Vec<(u32, u32, usize)> = Vec::new();
    visit_terminal(cells, root, &spelling, &mut keys)?;
    stack.push((root, start[root as usize], 0));
    while let Some(top) = stack.last_mut() {
        let (base, cursor, key_length) = *top;
        if base as usize >= cells.len() || cursor >= start[base as usize + 1] {
            stack.pop();
            continue;
        }
        top.1 += 1;
        let child_cell = children[cursor as usize];
        spelling.truncate(key_length);
        spelling.push((child_cell - base - 1) as u8);
        let child = cells[child_cell as usize][0];
        if child.cast_signed() < 0 {
            continue;
        }
        visit_terminal(cells, child, &spelling, &mut keys)?;
        stack.push((child, start[child as usize], spelling.len()));
    }
    Ok(keys)
}

fn count_terminals(cells: &[[u32; 2]]) -> usize {
    cells
        .iter()
        .enumerate()
        .filter(|&(index, &[value, check])| {
            index != 0 && check as usize == index && value.cast_signed() < 0
        })
        .count()
}

fn read_keys(path: &Path) -> Result<Vec<RawKey>, Error> {
    let bytes = fs::read(path).map_err(io_error(path))?;
    let end = HEADER_SIZE + u32_at(&bytes, ARRAY_BYTES_AT) as usize;
    let cells: Vec<[u32; 2]> = bytes[HEADER_SIZE..end]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|cell| [u32_at(cell, 0), u32_at(cell, 4)])
        .collect();
    let Some(&[root, _]) = cells.first() else {
        return Ok(Vec::new());
    };
    let keys = walk_trie(&cells, &child_index(&cells), root)?;
    if count_terminals(&cells) != keys.len() {
        return Err(Error::format(
            path,
            "the double array has keys that are not reachable",
        ));
    }
    Ok(keys)
}

struct MatrixFiles {
    path: PathBuf,
    matrix: Vec<u8>,
    templates: Vec<u8>,
    left_size: usize,
    right_size: usize,
}

fn plain_matrix_sizes(path: &Path, matrix: &[u8]) -> Result<(usize, usize), Error> {
    let size = |at: usize| {
        matrix
            .get(at..at + 2)
            .map(|pair| usize::from(u16_at(pair, 0)))
    };
    let (Some(left_size), Some(right_size)) = (size(0), size(2)) else {
        return Err(Error::format(path, "the file is shorter than its header"));
    };
    if left_size == 0 || right_size == 0 || 4 + left_size * right_size * 2 != matrix.len() {
        return Err(Error::format(
            path,
            "the file size does not match the header",
        ));
    }
    Ok((left_size, right_size))
}

fn load_matrix(directory: &Path) -> Result<MatrixFiles, Error> {
    let hot_path = directory.join(MATRIX_HOT_FILE);
    let int_templates_path = directory.join(MATRIX_ITP_FILE);
    let hot = hot_path.exists() && int_templates_path.exists();
    let path = if hot {
        hot_path
    } else {
        directory.join(MATRIX_BIN_FILE)
    };
    if !hot && !path.exists() {
        return Err(Error::Unsupported(
            "a dictionary without matrix.bin or matrix.hot and matrix.itp",
        ));
    }
    let matrix = fs::read(&path).map_err(io_error(&path))?;
    let templates = if hot {
        fs::read(&int_templates_path).map_err(io_error(&int_templates_path))?
    } else {
        Vec::new()
    };
    let (left_size, right_size) = if hot {
        let (left_size, right_size, _) =
            HotShape::parse(&matrix).map_err(|reason| Error::format(&path, reason))?;
        (left_size, right_size)
    } else {
        plain_matrix_sizes(&path, &matrix)?
    };
    if hot {
        let mapped = MappedIntTemplates::open(&int_templates_path)?;
        if mapped.left_size() != left_size || mapped.right_size() != right_size {
            return Err(Error::format(
                &int_templates_path,
                "the integer templates do not match the hot matrix",
            ));
        }
    }
    Ok(MatrixFiles {
        path,
        matrix,
        templates,
        left_size,
        right_size,
    })
}

fn check_sizes(
    files: &MatrixFiles,
    system: &SystemDictionary,
    unknown: &SystemDictionary,
) -> Result<(), Error> {
    let matches_matrix = |dictionary: &SystemDictionary| {
        dictionary.left_size() == files.left_size && dictionary.right_size() == files.right_size
    };
    if matches_matrix(system) && matches_matrix(unknown) {
        Ok(())
    } else {
        Err(Error::format(
            &files.path,
            "the matrix size does not match sys.dic or unk.dic",
        ))
    }
}

fn check_key_order(raw_keys: &[RawKey], system_path: &Path) -> Result<(), Error> {
    let mut position = 0u32;
    for raw in raw_keys {
        if raw.first_token != position || raw.count == 0 || raw.count > MAX_KEY_TOKENS {
            return Err(Error::format(
                system_path,
                "the tokens are not in key order, or a key has no token",
            ));
        }
        position += raw.count;
    }
    if position as usize != system_token_count(system_path)? {
        return Err(Error::format(system_path, "some tokens belong to no key"));
    }
    Ok(())
}

fn intern_system_tokens<'a>(
    model: &mut Model,
    interner: &mut Interner<'a>,
    system: &'a SystemDictionary,
    raw_keys: Vec<RawKey>,
) {
    for raw in raw_keys {
        model.counts.push(raw.count as u8);
        for index in raw.first_token..raw.first_token + raw.count {
            let (param, feature) = interner.intern(model, system, system.token(index as usize));
            model.token_params.push(param);
            model.token_features.push(feature);
        }
        model.keys.push(raw.key);
    }
}

fn intern_unknown_tokens<'a>(
    model: &mut Model,
    interner: &mut Interner<'a>,
    unknown: &'a SystemDictionary,
    chars: &CharProperty,
    unknown_path: &Path,
) -> Result<(), Error> {
    model.unknown_starts.push(0);
    for category in 0..chars.category_count() {
        let run = unknown.exact_match(chars.name(category)).ok_or_else(|| {
            Error::format(unknown_path, "a character category of char.uni is missing")
        })?;
        for index in run {
            let (param, feature) = interner.intern(model, unknown, unknown.token(index));
            model.unknown_params.push(param);
            model.unknown_features.push(feature);
        }
        model.unknown_starts.push(model.unknown_params.len() as u32);
    }
    Ok(())
}

pub fn read_dictionary(directory: &Path) -> Result<Model, Error> {
    let (bos_feature, unknown_feature) = dicrc_features(&directory.join(DICRC_FILE))?;
    let system_path = directory.join(SYS_DIC_FILE);
    let system = SystemDictionary::open(&system_path, DictionaryKind::System)?;
    let unknown_path = directory.join(UNK_DIC_FILE);
    let unknown = SystemDictionary::open(&unknown_path, DictionaryKind::Unknown)?;
    let chars = CharProperty::open(&directory.join(CHAR_TABLE_FILE))?;
    let files = load_matrix(directory)?;
    check_sizes(&files, &system, &unknown)?;
    let raw_keys = read_keys(&system_path)?;
    let mut model = Model {
        matrix: files.matrix,
        templates: files.templates,
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
    check_key_order(&raw_keys, &system_path)?;
    intern_system_tokens(&mut model, &mut interner, &system, raw_keys);
    intern_unknown_tokens(&mut model, &mut interner, &unknown, &chars, &unknown_path)?;
    Ok(model)
}

fn system_token_count(path: &Path) -> Result<usize, Error> {
    let mut header = [0u8; HEADER_SIZE];
    let mut file = fs::File::open(path).map_err(io_error(path))?;
    std::io::Read::read_exact(&mut file, &mut header).map_err(io_error(path))?;
    Ok(u32_at(&header, TOKEN_BYTES_AT) as usize / TOKEN_SIZE)
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
