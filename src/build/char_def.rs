use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use super::text::{BUF_SIZE, Lines, atoi, cstr, failure, tokenize_csv, tokenize2};
use crate::error::Error;

const CHAR_PROPERTY_DEF_DEFAULT: &str = "DEFAULT 1 0 0\nSPACE   0 1 0\n0x0020 SPACE\n";
pub(super) const UNK_DEF_DEFAULT: &str = "DEFAULT,0,0,0,*\nSPACE,0,0,0,*\n";
const TABLE_ENTRIES: usize = 0xffff;
const NAME_BYTES: usize = 32;
const TYPE_MASK_BITS: u32 = 18;
const TYPE_MASK: u32 = (1 << TYPE_MASK_BITS) - 1;
const DEFAULT_TYPE_SHIFT: u32 = TYPE_MASK_BITS;
const DEFAULT_TYPE_BITS: u32 = 0xff;
const LENGTH_SHIFT: u32 = 26;
const LENGTH_BITS: u32 = 0xf;
const GROUP_SHIFT: u32 = 30;
const INVOKE_SHIFT: u32 = 31;
const MAX_CATEGORIES: usize = 18;
const MAX_DEF_COLUMNS: usize = 512;
const COUNT_BYTES: usize = 4;
const ENTRY_BYTES: usize = 4;

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
        (self.type_mask & TYPE_MASK)
            | (self.default_type & DEFAULT_TYPE_BITS) << DEFAULT_TYPE_SHIFT
            | (self.length & LENGTH_BITS) << LENGTH_SHIFT
            | (self.group & 1) << GROUP_SHIFT
            | (self.invoke & 1) << INVOKE_SHIFT
    }
}

struct CodeRange {
    low: i32,
    high: i32,
    categories: Vec<Vec<u8>>,
}

fn parse_hex_code(text: &[u8], path: &Path) -> Result<i32, Error> {
    let no_hex = || {
        failure(
            path,
            format!("no hex value: {}", String::from_utf8_lossy(text)),
        )
    };
    let [b'0', b'x' | b'X', digits @ ..] = text else {
        return Err(no_hex());
    };
    if digits.is_empty() {
        return Err(no_hex());
    }
    let mut value: i32 = 0;
    for &digit in digits {
        let digit = char::from(digit).to_digit(16).ok_or_else(no_hex)?;
        value = value.wrapping_mul(16).wrapping_add(digit as i32);
    }
    Ok(value)
}

fn char_info_of_categories(
    categories: &[Vec<u8>],
    table: &BTreeMap<Vec<u8>, CharInfo>,
    path: &Path,
) -> Result<CharInfo, Error> {
    let undefined = |name: &[u8]| {
        failure(
            path,
            format!("category [{}] is undefined", String::from_utf8_lossy(name)),
        )
    };
    let first = categories
        .first()
        .ok_or_else(|| failure(path, "category size is empty"))?;
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
    source: &Path,
    categories: &BTreeMap<Vec<u8>, CharInfo>,
) -> Result<CodeRange, Error> {
    let (low, high) = match first.windows(2).position(|pair| pair == b"..") {
        Some(dots) => (&first[..dots], &first[dots + 2..]),
        None => (first, first),
    };
    let low = parse_hex_code(low, source)?;
    let high = parse_hex_code(high, source)?;
    let in_table = |code: i32| (0..TABLE_ENTRIES as i32).contains(&code);
    if !(in_table(low) && in_table(high) && low <= high) {
        return Err(failure(
            source,
            format!("range error: low={low} high={high}"),
        ));
    }
    let mut names = Vec::new();
    for &column in &columns[1..] {
        let name = cstr(line, column);
        if name.first() == Some(&b'#') {
            break;
        }
        if !categories.contains_key(name) {
            return Err(failure(
                source,
                format!("category [{}] is undefined", String::from_utf8_lossy(name)),
            ));
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

fn read_char_def(char_def: &Path) -> Result<CharDef, Error> {
    let mut lines = if char_def.is_file() {
        Lines::open(char_def, Some(BUF_SIZE))?
    } else {
        eprintln!(
            "{} is not found. minimum setting is used",
            char_def.display()
        );
        Lines::builtin(CHAR_PROPERTY_DEF_DEFAULT, char_def, Some(BUF_SIZE))
    };
    let source = lines.path().to_path_buf();
    let mut ranges: Vec<CodeRange> = Vec::new();
    let mut categories: BTreeMap<Vec<u8>, CharInfo> = BTreeMap::new();
    let mut order: Vec<Vec<u8>> = Vec::new();
    while let Some(line) = lines.next_line()? {
        if line[0] == 0 || line[0] == b'#' {
            continue;
        }
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let columns = tokenize2(line, b"\t ", MAX_DEF_COLUMNS);
        if columns.len() < 2 {
            return Err(failure(&source, format!("format error: {shown}")));
        }
        let first = cstr(line, columns[0]);
        if first.starts_with(b"0x") {
            ranges.push(parse_code_range(
                first,
                &columns,
                line,
                &source,
                &categories,
            )?);
        } else {
            if columns.len() < 4 {
                return Err(failure(&source, format!("format error: {shown}")));
            }
            if categories.contains_key(first) {
                return Err(failure(
                    &source,
                    format!(
                        "category {} is already defined",
                        String::from_utf8_lossy(first)
                    ),
                ));
            }
            let info = parse_category(&columns, line, order.len());
            categories.insert(first.to_vec(), info);
            order.push(first.to_vec());
        }
    }
    Ok(CharDef {
        ranges,
        categories,
        order,
    })
}

fn require_categories_complete(
    categories: &BTreeMap<Vec<u8>, CharInfo>,
    char_def: &Path,
    unk_def: &Path,
) -> Result<(), Error> {
    if categories.len() >= MAX_CATEGORIES {
        return Err(failure(
            char_def,
            format!("too many categories(>= {MAX_CATEGORIES})"),
        ));
    }
    for required in ["DEFAULT", "SPACE"] {
        if !categories.contains_key(required.as_bytes()) {
            return Err(failure(
                char_def,
                format!("category [{required}] is undefined"),
            ));
        }
    }

    let mut lines = if unk_def.is_file() {
        Lines::open(unk_def, Some(BUF_SIZE))?
    } else {
        eprintln!(
            "{} is not found. minimum setting is used.",
            unk_def.display()
        );
        Lines::builtin(UNK_DEF_DEFAULT, unk_def, Some(BUF_SIZE))
    };
    let unk_source = lines.path().to_path_buf();
    let mut unknown: HashSet<Vec<u8>> = HashSet::new();
    while let Some(line) = lines.next_line()? {
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let columns = tokenize_csv(line, 2);
        if columns.is_empty() {
            return Err(failure(&unk_source, format!("format error: {shown}")));
        }
        let key = cstr(line, columns[0]);
        if !categories.contains_key(key) {
            return Err(failure(
                &unk_source,
                format!(
                    "category [{}] is undefined in {}",
                    String::from_utf8_lossy(key),
                    char_def.display()
                ),
            ));
        }
        unknown.insert(key.to_vec());
    }
    for name in categories.keys() {
        if !unknown.contains(name) {
            return Err(failure(
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
    let mut table = vec![default; TABLE_ENTRIES];
    for range in &ranges {
        let info = char_info_of_categories(&range.categories, &categories, char_def)?;
        table[range.low as usize..=range.high as usize].fill(info);
    }

    let mut bytes =
        Vec::with_capacity(COUNT_BYTES + NAME_BYTES * order.len() + ENTRY_BYTES * TABLE_ENTRIES);
    bytes.extend_from_slice(&(categories.len() as u32).to_le_bytes());
    for name in &order {
        let mut padded = [0u8; NAME_BYTES];
        let length = name.len().min(NAME_BYTES - 1);
        padded[..length].copy_from_slice(&name[..length]);
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
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let categories = bytes
            .first_chunk::<4>()
            .map(|count| u32::from_le_bytes(*count) as usize)
            .ok_or_else(|| failure(path, "invalid file size"))?;
        let table_at = COUNT_BYTES + NAME_BYTES * categories;
        if bytes.len() != table_at + ENTRY_BYTES * TABLE_ENTRIES {
            return Err(failure(path, "invalid file size"));
        }
        let names = bytes[COUNT_BYTES..table_at]
            .as_chunks::<NAME_BYTES>()
            .0
            .iter()
            .map(|name| cstr(name, 0).to_vec())
            .collect();
        let table = bytes[table_at..]
            .as_chunks::<ENTRY_BYTES>()
            .0
            .iter()
            .map(|entry| u32::from_le_bytes(*entry))
            .collect();
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
