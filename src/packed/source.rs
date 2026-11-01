use crate::hash::FastMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::model::{Model, Param};
use crate::Error;
use crate::char_property::CharProperty;
use crate::dict_files::{
    CHAR_TABLE_FILE, DICRC_FILE, MATRIX_BIN_FILE, MATRIX_HOT_FILE, MATRIX_ITP_FILE, SYS_DIC_FILE,
    UNK_DIC_FILE,
};
use crate::error::{IoAction, io_error};
use crate::hot_shape::HotShape;
use crate::int_templates::MappedIntTemplates;
use crate::system_dictionary::{
    ARRAY_BYTES_AT, DictionaryKind, HEADER_SIZE, SystemDictionary, TOKEN_BYTES_AT, TOKEN_SIZE,
    TokenRecord, VALUE_SIZE_BITS,
};
use crate::wire::{u16_at, u32_at};

const MAX_ROOT_FANOUT: u32 = 256;
const MAX_KEY_BYTES: usize = 1 << 12;
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
    let bytes = fs::read(path).map_err(io_error(IoAction::Read, path))?;
    let mut values: Vec<(&[u8], &[u8])> = Vec::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b';' || line[0] == b'#' {
            continue;
        }
        let Some((key, value)) = key_value(line) else {
            return Err(Error::format(path, "a line has no '='"));
        };
        values.push((key, value));
    }
    let find = |wanted: &[u8]| {
        values
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|&(_, value)| value)
    };
    let text = |value: &[u8]| {
        String::from_utf8(value.to_vec())
            .map_err(|_| Error::format(path, "bos-feature or unk-feature is not valid UTF-8"))
    };
    let unknown = find(b"unk-feature")
        .filter(|value| !value.is_empty())
        .map(text)
        .transpose()?;
    let bos = find(b"bos-feature")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::format(path, "bos-feature is missing"))?;
    Ok((text(bos)?, unknown))
}

struct RawKey {
    key: String,
    first_token: u32,
    count: u32,
}

#[derive(Clone, Copy)]
struct Node(u32);

struct DoubleArray {
    cells: Vec<[u32; 2]>,
    start: Vec<u32>,
    children: Vec<u32>,
}

fn is_child(index: usize, check: u32) -> bool {
    check != 0 && index > check as usize && index - check as usize <= MAX_ROOT_FANOUT as usize
}

fn edge_byte(parent: Node, child_cell: u32) -> u8 {
    let offset = child_cell - parent.0 - 1;
    u8::try_from(offset).expect("is_child admits only offsets 0..=255")
}

impl DoubleArray {
    fn new(cells: Vec<[u32; 2]>) -> Self {
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
        Self {
            cells,
            start,
            children,
        }
    }

    fn node(&self, base: u32) -> Option<Node> {
        ((base as usize) < self.cells.len()).then_some(Node(base))
    }

    fn child_cells(&self, node: Node) -> &[u32] {
        let from = self.start[node.0 as usize] as usize;
        let to = self.start[node.0 as usize + 1] as usize;
        &self.children[from..to]
    }

    fn terminal_value(&self, node: Node) -> Option<u32> {
        let [value, check] = self.cells[node.0 as usize];
        (check == node.0 && value.cast_signed() < 0).then_some(!value)
    }

    fn count_terminals(&self) -> usize {
        self.cells
            .iter()
            .enumerate()
            .filter(|&(index, &[value, check])| {
                index != 0 && check as usize == index && value.cast_signed() < 0
            })
            .count()
    }
}

struct Frame<'a> {
    node: Node,
    children: std::slice::Iter<'a, u32>,
    key_length: usize,
}

struct Walk<'a> {
    array: &'a DoubleArray,
    path: &'a Path,
    seen: Vec<bool>,
    spelling: Vec<u8>,
    keys: Vec<RawKey>,
}

impl<'a> Walk<'a> {
    fn new(array: &'a DoubleArray, path: &'a Path) -> Self {
        Self {
            array,
            path,
            seen: vec![false; array.cells.len()],
            spelling: Vec::new(),
            keys: Vec::new(),
        }
    }

    fn enter(&mut self, base: u32) -> Result<Frame<'a>, Error> {
        let node = self.array.node(base).ok_or_else(|| {
            Error::format(self.path, "a double array base lies outside the array")
        })?;
        if self.spelling.len() > MAX_KEY_BYTES {
            return Err(Error::format(self.path, "a sys.dic key is too long"));
        }
        if std::mem::replace(&mut self.seen[node.0 as usize], true) {
            return Err(Error::format(
                self.path,
                "a double array node is reached twice",
            ));
        }
        self.record_terminal(node)?;
        Ok(Frame {
            node,
            children: self.array.child_cells(node).iter(),
            key_length: self.spelling.len(),
        })
    }

    fn record_terminal(&mut self, node: Node) -> Result<(), Error> {
        let Some(value) = self.array.terminal_value(node) else {
            return Ok(());
        };
        let key = String::from_utf8(self.spelling.clone())
            .map_err(|_| Error::format(self.path, "a sys.dic key is not valid UTF-8"))?;
        self.keys.push(RawKey {
            key,
            first_token: value >> VALUE_SIZE_BITS,
            count: value & MAX_KEY_TOKENS,
        });
        Ok(())
    }

    fn run(mut self, root: u32) -> Result<Vec<RawKey>, Error> {
        let mut stack = vec![self.enter(root)?];
        while let Some(frame) = stack.last_mut() {
            let Some(&child_cell) = frame.children.next() else {
                stack.pop();
                continue;
            };
            let (node, key_length) = (frame.node, frame.key_length);
            self.spelling.truncate(key_length);
            self.spelling.push(edge_byte(node, child_cell));
            let value = self.array.cells[child_cell as usize][0];
            if value.cast_signed() >= 0 {
                stack.push(self.enter(value)?);
            }
        }
        Ok(self.keys)
    }
}

struct SystemKeys {
    keys: Vec<RawKey>,
    token_count: usize,
}

fn read_keys(path: &Path) -> Result<SystemKeys, Error> {
    let bytes = fs::read(path).map_err(io_error(IoAction::Read, path))?;
    let truncated = || Error::format(path, "the file is shorter than its header");
    let header = bytes.get(..HEADER_SIZE).ok_or_else(truncated)?;
    let token_count = u32_at(header, TOKEN_BYTES_AT) as usize / TOKEN_SIZE;
    let array_end = HEADER_SIZE
        .checked_add(u32_at(header, ARRAY_BYTES_AT) as usize)
        .ok_or_else(truncated)?;
    let cells: Vec<[u32; 2]> = bytes
        .get(HEADER_SIZE..array_end)
        .ok_or_else(truncated)?
        .as_chunks::<8>()
        .0
        .iter()
        .map(|cell| [u32_at(cell, 0), u32_at(cell, 4)])
        .collect();
    let Some(&[root, _]) = cells.first() else {
        return Ok(SystemKeys {
            keys: Vec::new(),
            token_count,
        });
    };
    let array = DoubleArray::new(cells);
    let keys = Walk::new(&array, path).run(root)?;
    if array.count_terminals() != keys.len() {
        return Err(Error::format(
            path,
            "the double array has keys that are not reachable",
        ));
    }
    Ok(SystemKeys { keys, token_count })
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
    let matrix = fs::read(&path).map_err(io_error(IoAction::Read, &path))?;
    let templates = if hot {
        fs::read(&int_templates_path).map_err(io_error(IoAction::Read, &int_templates_path))?
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

fn check_key_order(system_keys: &SystemKeys, system_path: &Path) -> Result<(), Error> {
    let out_of_order = || {
        Error::format(
            system_path,
            "the tokens are not in key order, or a key has no token",
        )
    };
    let mut position = 0u32;
    for raw in &system_keys.keys {
        if raw.first_token != position || raw.count == 0 || raw.count > MAX_KEY_TOKENS {
            return Err(out_of_order());
        }
        position = position.checked_add(raw.count).ok_or_else(out_of_order)?;
    }
    if position as usize != system_keys.token_count {
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
    let system_keys = read_keys(&system_path)?;
    let mut model = Model {
        matrix: files.matrix,
        templates: files.templates,
        char_table: fs::read(directory.join(CHAR_TABLE_FILE))
            .map_err(io_error(IoAction::Read, &directory.join(CHAR_TABLE_FILE)))?,
        bos_feature,
        unknown_feature,
        ..Model::default()
    };
    let mut interner = Interner {
        params: FastMap::default(),
        features: FastMap::default(),
    };
    check_key_order(&system_keys, &system_path)?;
    intern_system_tokens(&mut model, &mut interner, &system, system_keys.keys);
    intern_unknown_tokens(&mut model, &mut interner, &unknown, &chars, &unknown_path)?;
    Ok(model)
}

struct Interner<'a> {
    params: FastMap<Param, u32>,
    features: FastMap<&'a str, u32>,
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
