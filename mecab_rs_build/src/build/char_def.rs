use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::text::{LINE_BUFFER_BYTES, Lines, MAX_DEF_COLUMNS, atoi, cstr, tokenize_csv, tokenize2};
use crate::tool_error::{Position, build_error, io_error};
use mecab_rs::__private::char_layout::{
    CATEGORY_NAME_BYTES, CHAR_BIN_COUNT_BYTES, CHAR_BIN_ENTRIES, CHAR_BIN_ENTRY_BYTES,
    DEFAULT_TYPE_SHIFT, MAX_CATEGORIES, TYPE_MASK, pack_char_info,
};
use mecab_rs::__private::hash::FastSet;
use mecab_rs::__private::in_range_bytes::{u32_at, u32_values};
use mecab_rs::Error;
use mecab_rs::IoAction;

const CHAR_PROPERTY_DEF_DEFAULT: &str = "DEFAULT 1 0 0\nSPACE   0 1 0\n0x0020 SPACE\n";
pub(super) const UNK_DEF_DEFAULT: &str = "DEFAULT,0,0,0,*\nSPACE,0,0,0,*\n";

#[derive(Clone, Copy)]
struct CharInfo {
    type_mask: u32,
    default_type: u32,
    length: u32,
    group: u32,
    invoke: u32,
}

impl CharInfo {
    fn pack(self) -> u32 {
        pack_char_info(
            self.type_mask,
            self.default_type,
            self.length,
            self.group,
            self.invoke,
        )
    }
}

struct CodeRange {
    low: i32,
    high: i32,
    categories: Vec<Vec<u8>>,
}

fn parse_hex_code(text: &[u8], at: Position) -> Result<i32, Error> {
    let no_hex = || at.build(format!("no hex value: {}", String::from_utf8_lossy(text)));
    let [b'0', b'x' | b'X', digits @ ..] = text else {
        return Err(no_hex());
    };
    if digits.is_empty() {
        return Err(no_hex());
    }
    let mut value: i32 = 0;
    for &digit in digits {
        let digit = char::from(digit).to_digit(16).ok_or_else(no_hex)?;
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit as i32))
            .ok_or_else(|| {
                at.build(format!(
                    "hex value overflows: {}",
                    String::from_utf8_lossy(text)
                ))
            })?;
    }
    Ok(value)
}

fn char_info_of_categories(
    categories: &[Vec<u8>],
    table: &BTreeMap<Vec<u8>, CharInfo>,
    path: &Path,
) -> Result<CharInfo, Error> {
    let undefined = |name: &[u8]| {
        build_error(
            path,
            format!("category [{}] is undefined", String::from_utf8_lossy(name)),
        )
    };
    let first = categories
        .first()
        .ok_or_else(|| build_error(path, "category size is empty"))?;
    let mut base = *table.get(first).ok_or_else(|| undefined(first))?;
    for name in categories {
        let info = table.get(name).ok_or_else(|| undefined(name))?;
        base.type_mask = base
            .type_mask
            .wrapping_add(1u32.wrapping_shl(info.default_type))
            & TYPE_MASK;
    }
    Ok(base)
}

struct CharDef {
    ranges: Vec<CodeRange>,
    categories: BTreeMap<Vec<u8>, CharInfo>,
    order: Vec<Vec<u8>>,
}

fn parse_code_range(
    first: &[u8],
    columns: &[usize],
    line: &[u8],
    at: Position,
    categories: &BTreeMap<Vec<u8>, CharInfo>,
) -> Result<CodeRange, Error> {
    let (low, high) = match first.windows(2).position(|pair| pair == b"..") {
        Some(dots) => (&first[..dots], &first[dots + 2..]),
        None => (first, first),
    };
    let low = parse_hex_code(low, at)?;
    let high = parse_hex_code(high, at)?;
    let in_table = |code: i32| (0..CHAR_BIN_ENTRIES as i32).contains(&code);
    if !(in_table(low) && in_table(high) && low <= high) {
        return Err(at.build(format!("range error: low={low} high={high}")));
    }
    let mut names = Vec::new();
    for &column in &columns[1..] {
        let name = cstr(line, column);
        if name.first() == Some(&b'#') {
            break;
        }
        if !categories.contains_key(name) {
            return Err(at.build(format!(
                "category [{}] is undefined",
                String::from_utf8_lossy(name)
            )));
        }
        names.push(name.to_vec());
    }
    Ok(CodeRange {
        low,
        high,
        categories: names,
    })
}

fn parse_category(columns: &[usize], line: &[u8], definition_index: usize) -> CharInfo {
    const INVOKE_COLUMN: usize = 1;
    const GROUP_COLUMN: usize = 2;
    const LENGTH_COLUMN: usize = 3;
    CharInfo {
        type_mask: 0,
        default_type: definition_index as u32,
        length: atoi(cstr(line, columns[LENGTH_COLUMN])) as u32,
        group: atoi(cstr(line, columns[GROUP_COLUMN])) as u32,
        invoke: atoi(cstr(line, columns[INVOKE_COLUMN])) as u32,
    }
}

const REQUIRED_CATEGORIES: [&str; 2] = ["DEFAULT", "SPACE"];

fn define_category(
    columns: &[usize],
    line: &[u8],
    at: Position,
    shown: &str,
    char_def: &mut CharDef,
) -> Result<(), Error> {
    if columns.len() < 4 {
        return Err(at.build(format!("format error: {shown}")));
    }
    let name = cstr(line, columns[0]);
    if name.len() >= CATEGORY_NAME_BYTES {
        return Err(at.build(format!(
            "category name is longer than {} bytes: {}",
            CATEGORY_NAME_BYTES - 1,
            String::from_utf8_lossy(name)
        )));
    }
    if char_def.categories.contains_key(name) {
        return Err(at.build(format!(
            "category {} is already defined",
            String::from_utf8_lossy(name)
        )));
    }
    let info = parse_category(columns, line, char_def.order.len());
    char_def.categories.insert(name.to_vec(), info);
    char_def.order.push(name.to_vec());
    Ok(())
}

fn read_char_def(char_def: &Path) -> Result<CharDef, Error> {
    let mut lines = if char_def.is_file() {
        Lines::open(char_def, Some(LINE_BUFFER_BYTES))?
    } else {
        eprintln!(
            "{} is not found. minimum setting is used",
            char_def.display()
        );
        Lines::builtin(CHAR_PROPERTY_DEF_DEFAULT, char_def, Some(LINE_BUFFER_BYTES))
    };
    let source = lines.path().to_path_buf();
    let mut result = CharDef {
        ranges: Vec::new(),
        categories: BTreeMap::new(),
        order: Vec::new(),
    };
    while let Some((number, line)) = lines.next_line()? {
        if line[0] == 0 || line[0] == b'#' {
            continue;
        }
        let at = Position::new(&source, number);
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let columns = tokenize2(line, b"\t ", MAX_DEF_COLUMNS);
        if columns.len() < 2 {
            return Err(at.build(format!("format error: {shown}")));
        }
        let first = cstr(line, columns[0]);
        if first.starts_with(b"0x") {
            let range = parse_code_range(first, &columns, line, at, &result.categories)?;
            result.ranges.push(range);
        } else {
            define_category(&columns, line, at, &shown, &mut result)?;
        }
    }
    Ok(result)
}

fn require_core_categories(
    categories: &BTreeMap<Vec<u8>, CharInfo>,
    char_def: &Path,
) -> Result<(), Error> {
    if categories.len() >= MAX_CATEGORIES {
        return Err(build_error(
            char_def,
            format!("too many categories(>= {MAX_CATEGORIES})"),
        ));
    }
    for required in REQUIRED_CATEGORIES {
        if !categories.contains_key(required.as_bytes()) {
            return Err(build_error(
                char_def,
                format!("category [{required}] is undefined"),
            ));
        }
    }
    Ok(())
}

fn read_unknown_categories(
    unk_def: &Path,
    char_def: &Path,
    categories: &BTreeMap<Vec<u8>, CharInfo>,
) -> Result<(FastSet<Vec<u8>>, PathBuf), Error> {
    let mut lines = if unk_def.is_file() {
        Lines::open(unk_def, Some(LINE_BUFFER_BYTES))?
    } else {
        eprintln!(
            "{} is not found. minimum setting is used.",
            unk_def.display()
        );
        Lines::builtin(UNK_DEF_DEFAULT, unk_def, Some(LINE_BUFFER_BYTES))
    };
    let unk_source = lines.path().to_path_buf();
    let mut unknown: FastSet<Vec<u8>> = FastSet::default();
    while let Some((number, line)) = lines.next_line()? {
        let at = Position::new(&unk_source, number);
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let columns = tokenize_csv(line, 2);
        if columns.is_empty() {
            return Err(at.build(format!("format error: {shown}")));
        }
        let key = cstr(line, columns[0]);
        if !categories.contains_key(key) {
            return Err(at.build(format!(
                "category [{}] is undefined in {}",
                String::from_utf8_lossy(key),
                char_def.display()
            )));
        }
        unknown.insert(key.to_vec());
    }
    Ok((unknown, unk_source))
}

fn require_categories_complete(
    categories: &BTreeMap<Vec<u8>, CharInfo>,
    char_def: &Path,
    unk_def: &Path,
) -> Result<(), Error> {
    require_core_categories(categories, char_def)?;
    let (unknown, unk_source) = read_unknown_categories(unk_def, char_def, categories)?;
    for name in categories.keys() {
        if !unknown.contains(name) {
            return Err(build_error(
                &unk_source,
                format!(
                    "category [{}] is undefined in {}",
                    String::from_utf8_lossy(name),
                    unk_source.display()
                ),
            ));
        }
    }
    Ok(())
}

pub(super) fn compile(char_def: &Path, unk_def: &Path) -> Result<Vec<u8>, Error> {
    let CharDef {
        ranges,
        categories,
        order,
    } = read_char_def(char_def)?;
    require_categories_complete(&categories, char_def, unk_def)?;

    let default = char_info_of_categories(&[b"DEFAULT".to_vec()], &categories, char_def)?;
    let mut table = vec![default; CHAR_BIN_ENTRIES];
    for range in &ranges {
        let info = char_info_of_categories(&range.categories, &categories, char_def)?;
        table[range.low as usize..=range.high as usize].fill(info);
    }

    let mut bytes = Vec::with_capacity(
        CHAR_BIN_COUNT_BYTES
            + CATEGORY_NAME_BYTES * order.len()
            + CHAR_BIN_ENTRY_BYTES * CHAR_BIN_ENTRIES,
    );
    bytes.extend_from_slice(&(categories.len() as u32).to_le_bytes());
    for name in &order {
        let mut padded = [0u8; CATEGORY_NAME_BYTES];
        debug_assert!(name.len() < CATEGORY_NAME_BYTES);
        padded[..name.len()].copy_from_slice(name);
        bytes.extend_from_slice(&padded);
    }
    for info in &table {
        bytes.extend_from_slice(&info.pack().to_le_bytes());
    }
    Ok(bytes)
}

fn utf8_to_ucs2(text: &[u8], begin: usize, end: usize) -> (u32, usize) {
    let len = end.wrapping_sub(begin);
    let byte = |index: usize| u32::from(text.get(begin + index).copied().unwrap_or(0));
    if byte(0) < 0x80 {
        (byte(0), 1)
    } else if len >= 2 && byte(0) & 0xe0 == 0xc0 {
        ((byte(0) & 0x1f) << 6 | (byte(1) & 0x3f), 2)
    } else if len >= 3 && byte(0) & 0xf0 == 0xe0 {
        (
            (byte(0) & 0x0f) << 12 | (byte(1) & 0x3f) << 6 | (byte(2) & 0x3f),
            3,
        )
    } else if len >= 4 && byte(0) & 0xf8 == 0xf0 {
        (0, 4)
    } else if len >= 5 && byte(0) & 0xfc == 0xf8 {
        (0, 5)
    } else if len >= 6 && byte(0) & 0xfe == 0xfc {
        (0, 6)
    } else {
        (0, 1)
    }
}

pub(crate) struct CharProperty {
    names: Vec<Vec<u8>>,
    table: Vec<u32>,
}

impl CharProperty {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(io_error(IoAction::Read, path))?;
        let categories = (bytes.len() >= CHAR_BIN_COUNT_BYTES)
            .then(|| u32_at(&bytes, 0) as usize)
            .ok_or_else(|| build_error(path, "invalid file size"))?;
        let table_at = CHAR_BIN_COUNT_BYTES + CATEGORY_NAME_BYTES * categories;
        if bytes.len() != table_at + CHAR_BIN_ENTRY_BYTES * CHAR_BIN_ENTRIES {
            return Err(build_error(path, "invalid file size"));
        }
        let names = bytes[CHAR_BIN_COUNT_BYTES..table_at]
            .as_chunks::<CATEGORY_NAME_BYTES>()
            .0
            .iter()
            .map(|name| cstr(name, 0).to_vec())
            .collect();
        let table = u32_values(&bytes[table_at..]).collect();
        Ok(Self { names, table })
    }

    pub(crate) fn id(&self, name: &[u8]) -> Option<usize> {
        self.names.iter().position(|candidate| candidate == name)
    }

    pub(crate) fn category_count(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn name(&self, index: usize) -> &[u8] {
        &self.names[index]
    }

    pub(crate) fn char_info(&self, text: &[u8], begin: usize, end: usize) -> (u32, usize) {
        let (code, length) = utf8_to_ucs2(text, begin, end);
        (self.table.get(code as usize).copied().unwrap_or(0), length)
    }

    pub(crate) fn default_type(&self, text: &[u8]) -> u8 {
        (self.char_info(text, 0, text.len()).0 >> DEFAULT_TYPE_SHIFT) as u8
    }
}
