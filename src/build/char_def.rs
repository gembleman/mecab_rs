
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use super::text::{BUF_SIZE, Lines, atoi, cstr, failure, tokenize_csv, tokenize2};
use crate::error::Error;

const CHAR_PROPERTY_DEF_DEFAULT: &str = "DEFAULT 1 0 0\nSPACE   0 1 0\n0x0020 SPACE\n";
pub(super) const UNK_DEF_DEFAULT: &str = "DEFAULT,0,0,0,*\nSPACE,0,0,0,*\n";
const TABLE_ENTRIES: usize = 0xffff;
const NAME_BYTES: usize = 32;
const TYPE_MASK: u32 = (1 << 18) - 1;

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
            | (self.default_type & 0xff) << 18
            | (self.length & 0xf) << 26
            | (self.group & 1) << 30
            | (self.invoke & 1) << 31
    }
}

struct Range {
    low: i32,
    high: i32,
    categories: Vec<Vec<u8>>,
}

fn atohex(text: &[u8], path: &Path) -> Result<i32, Error> {
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

fn encode(
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

pub(super) fn compile(char_def: &Path, unk_def: &Path) -> Result<Vec<u8>, Error> {
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
    let mut ranges: Vec<Range> = Vec::new();
    let mut categories: BTreeMap<Vec<u8>, CharInfo> = BTreeMap::new();
    let mut order: Vec<Vec<u8>> = Vec::new();
    while let Some(line) = lines.next_line()? {
        if line[0] == 0 || line[0] == b'#' {
            continue;
        }
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let columns = tokenize2(line, b"\t ", 512);
        if columns.len() < 2 {
            return Err(failure(&source, format!("format error: {shown}")));
        }
        let first = cstr(line, columns[0]);
        if first.starts_with(b"0x") {
            let (low, high) = match first.windows(2).position(|pair| pair == b"..") {
                Some(at) => (&first[..at], &first[at + 2..]),
                None => (first, first),
            };
            let low = atohex(low, &source)?;
            let high = atohex(high, &source)?;
            if !((0..0xffff).contains(&low) && (0..0xffff).contains(&high) && low <= high) {
                return Err(failure(
                    &source,
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
                        &source,
                        format!("category [{}] is undefined", String::from_utf8_lossy(name)),
                    ));
                }
                names.push(name.to_vec());
            }
            ranges.push(Range {
                low,
                high,
                categories: names,
            });
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
            let info = CharInfo {
                type_mask: 0,
                default_type: order.len() as u32,
                length: atoi(cstr(line, columns[3])) as u32,
                group: atoi(cstr(line, columns[2])) as u32,
                invoke: atoi(cstr(line, columns[1])) as u32,
            };
            categories.insert(first.to_vec(), info);
            order.push(first.to_vec());
        }
    }

    if categories.len() >= 18 {
        return Err(failure(char_def, "too many categories(>= 18)"));
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

    let default = encode(&[b"DEFAULT".to_vec()], &categories, char_def)?;
    let mut table = vec![default; TABLE_ENTRIES];
    for range in &ranges {
        let info = encode(&range.categories, &categories, char_def)?;
        table[range.low as usize..=range.high as usize].fill(info);
    }

    let mut bytes = Vec::with_capacity(4 + NAME_BYTES * order.len() + 4 * TABLE_ENTRIES);
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
        let table_at = 4 + NAME_BYTES * categories;
        if bytes.len() != table_at + 4 * 0xffff {
            return Err(failure(path, "invalid file size"));
        }
        let names = bytes[4..table_at]
            .as_chunks::<NAME_BYTES>()
            .0
            .iter()
            .map(|name| cstr(name, 0).to_vec())
            .collect();
        let table = bytes[table_at..]
            .as_chunks::<4>()
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
        let len = end.wrapping_sub(begin);
        let byte = |index: usize| u32::from(text.get(begin + index).copied().unwrap_or(0));
        let (code, length) = if byte(0) < 0x80 {
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
        };
        (self.table.get(code as usize).copied().unwrap_or(0), length)
    }

    pub(crate) fn default_type(&self, text: &[u8]) -> u8 {
        (self.char_info(text, 0, text.len()).0 >> 18) as u8
    }
}
