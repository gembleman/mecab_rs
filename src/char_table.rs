use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::path::Path;

use crate::build::write_file;
use crate::error::Error;

pub(crate) const CHAR_TABLE_FILE: &str = "char.uni";
const CHAR_BIN_FILE: &str = "char.bin";
const CHAR_DEF_FILE: &str = "char.def";

pub(crate) const MAGIC: &[u8; 4] = b"MXCU";
pub(crate) const VERSION: u32 = 1;
pub(crate) const HEADER_BYTES: usize = 16;
pub(crate) const NAME_BYTES: usize = 32;
pub(crate) const PALETTE_ENTRY_BYTES: usize = 8;
pub(crate) const MAX_PALETTE: usize = 256;
pub(crate) const CODE_POINTS: usize = 0x11_0000;
pub(crate) const BMP_LEN: usize = 0x1_0000;
pub(crate) const EXTENDER_FLAG: u32 = 1;

const BIN_TABLE_ENTRIES: usize = 0xffff;
pub(crate) const TYPE_BITS: u32 = 18;
pub(crate) const DEFAULT_TYPE_SHIFT: u32 = TYPE_BITS;
pub(crate) const LENGTH_SHIFT: u32 = TYPE_BITS + 8;
pub(crate) const GROUP_SHIFT: u32 = LENGTH_SHIFT + 4;
pub(crate) const INVOKE_SHIFT: u32 = GROUP_SHIFT + 1;
const MAX_CATEGORIES: usize = 18;

const DEFAULT_CODE: usize = 0x0000;
const IDEOGRAPH_CODE: usize = 0x3400;
const COMPATIBILITY_IDEOGRAPH_CODE: usize = 0xf900;
const HIRAGANA_CODE: usize = 0x3042;
const KATAKANA_CODE: usize = 0x30a2;

const IDEOGRAPHIC_PLANES: RangeInclusive<usize> = 0x2_0000..=0x3_ffff;
const COMPATIBILITY_IDEOGRAPHS_SUPPLEMENT: RangeInclusive<usize> = 0x2_f800..=0x2_fa1f;
const KANA_BLOCKS: RangeInclusive<usize> = 0x1_aff0..=0x1_b16f;
const KATAKANA_RANGES: [RangeInclusive<usize>; 5] = [
    0x1_aff0..=0x1_afff,
    0x1_b000..=0x1_b000,
    0x1_b120..=0x1_b122,
    0x1_b155..=0x1_b155,
    0x1_b164..=0x1_b167,
];
const EXTENDER_RANGES: [RangeInclusive<usize>; 3] = [
    0xe_0100..=0xe_01ef,
    0x1_f3fb..=0x1_f3ff,
    0xe_0020..=0xe_007f,
];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Entry {
    pub(crate) info: u32,
    pub(crate) flags: u32,
}

pub struct CharTable {
    names: Vec<Vec<u8>>,
    palette: Vec<Entry>,
    table: Vec<u8>,
}

fn invalid(file: &Path, reason: &'static str) -> Error {
    Error::Format {
        file: file.to_path_buf(),
        reason,
    }
}

fn pack(type_mask: u32, default_type: u32, length: u32, group: u32, invoke: u32) -> u32 {
    (type_mask & ((1 << TYPE_BITS) - 1))
        | (default_type & 0xff) << DEFAULT_TYPE_SHIFT
        | (length & 0xf) << LENGTH_SHIFT
        | (group & 1) << GROUP_SHIFT
        | (invoke & 1) << INVOKE_SHIFT
}

fn atoi(text: &[u8]) -> u32 {
    let mut rest = text;
    while let [first, tail @ ..] = rest
        && first.is_ascii_whitespace()
    {
        rest = tail;
    }
    let negative = match rest {
        [b'-', tail @ ..] => {
            rest = tail;
            true
        }
        [b'+', tail @ ..] => {
            rest = tail;
            false
        }
        _ => false,
    };
    let mut value: u32 = 0;
    for digit in rest.iter().take_while(|byte| byte.is_ascii_digit()) {
        value = value.wrapping_mul(10).wrapping_add(u32::from(digit - b'0'));
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

fn atohex(text: &[u8]) -> Result<u32, &'static str> {
    const NO_HEX: &str = "no hex value";
    let [b'0', b'x' | b'X', digits @ ..] = text else {
        return Err(NO_HEX);
    };
    if digits.is_empty() {
        return Err(NO_HEX);
    }
    let mut value: u32 = 0;
    for &digit in digits {
        let digit = char::from(digit).to_digit(16).ok_or(NO_HEX)?;
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit))
            .filter(|&value| value <= 0x10_ffff)
            .ok_or("code point above U+10FFFF")?;
    }
    Ok(value)
}

struct DefRange {
    low: u32,
    high: u32,
    info: u32,
}

struct CharDef {
    names: Vec<Vec<u8>>,
    ranges: Vec<DefRange>,
}

fn parse_char_def(bytes: &[u8], file: &Path) -> Result<CharDef, Error> {
    let fail = |line: usize, reason: String| Error::CharDef {
        file: file.to_path_buf(),
        line,
        reason,
    };
    let mut categories: HashMap<&[u8], (u32, u32, u32, u32)> = HashMap::new();
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut ranges = Vec::new();
    for (index, raw) in bytes.split(|&byte| byte == b'\n').enumerate() {
        let number = index + 1;
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let columns: Vec<&[u8]> = line
            .split(|&byte| byte == b'\t' || byte == b' ')
            .filter(|column| !column.is_empty())
            .collect();
        if columns.len() < 2 {
            return Err(fail(number, "format error".into()));
        }
        if columns[0].starts_with(b"0x") {
            let (low, high) = match columns[0].windows(2).position(|pair| pair == b"..") {
                Some(at) => (&columns[0][..at], &columns[0][at + 2..]),
                None => (columns[0], columns[0]),
            };
            let low = atohex(low).map_err(|reason| fail(number, reason.into()))?;
            let high = atohex(high).map_err(|reason| fail(number, reason.into()))?;
            if low > high {
                return Err(fail(number, format!("range error: low={low} high={high}")));
            }
            let mut chosen = Vec::new();
            for &column in columns[1..].iter().take_while(|column| column[0] != b'#') {
                let Some(&category) = categories.get(column) else {
                    let name = String::from_utf8_lossy(column);
                    return Err(fail(number, format!("category [{name}] is undefined")));
                };
                chosen.push(category);
            }
            let Some(&(id, invoke, group, length)) = chosen.first() else {
                return Err(fail(number, "category size is empty".into()));
            };
            let type_mask = chosen
                .iter()
                .fold(0u32, |mask, category| mask.wrapping_add(1 << category.0));
            ranges.push(DefRange {
                low,
                high,
                info: pack(type_mask, id, length, group, invoke),
            });
        } else {
            if columns.len() < 4 {
                return Err(fail(number, "format error".into()));
            }
            if categories.contains_key(columns[0]) {
                return Err(fail(number, "category is already defined".into()));
            }
            let id = names.len() as u32;
            let (invoke, group, length) = (atoi(columns[1]), atoi(columns[2]), atoi(columns[3]));
            categories.insert(columns[0], (id, invoke, group, length));
            names.push(columns[0].to_vec());
        }
    }
    if names.len() >= MAX_CATEGORIES {
        return Err(fail(0, "too many categories(>= 18)".into()));
    }
    for required in [&b"DEFAULT"[..], b"SPACE"] {
        if !categories.contains_key(required) {
            let name = String::from_utf8_lossy(required);
            return Err(fail(0, format!("category [{name}] is undefined")));
        }
    }
    Ok(CharDef { names, ranges })
}

struct Interner {
    palette: Vec<Entry>,
    index: HashMap<Entry, u8>,
}

impl Interner {
    fn intern(&mut self, entry: Entry, file: &Path) -> Result<u8, Error> {
        if let Some(&index) = self.index.get(&entry) {
            return Ok(index);
        }
        let index = u8::try_from(self.palette.len())
            .map_err(|_| invalid(file, "more than 256 distinct character entries"))?;
        self.palette.push(entry);
        self.index.insert(entry, index);
        Ok(index)
    }
}

impl CharTable {
    fn build(bin: &[u8], bin_path: &Path, def: Option<(&[u8], &Path)>) -> Result<Self, Error> {
        let broken = |reason| invalid(bin_path, reason);
        let Some(&count) = bin.first_chunk() else {
            return Err(broken("the file is shorter than its header"));
        };
        let categories = u32::from_le_bytes(count) as usize;
        let table_start = categories
            .checked_mul(NAME_BYTES)
            .and_then(|names| names.checked_add(4))
            .ok_or_else(|| broken("the file size does not match the header"))?;
        if table_start.checked_add(4 * BIN_TABLE_ENTRIES) != Some(bin.len()) {
            return Err(broken("the file size does not match the header"));
        }
        if categories == 0 {
            return Err(broken("the file has no character categories"));
        }
        let names: Vec<Vec<u8>> = bin[4..table_start]
            .as_chunks::<NAME_BYTES>()
            .0
            .iter()
            .map(|name| {
                let length = name.iter().position(|&byte| byte == 0);
                name[..length.unwrap_or(NAME_BYTES)].to_vec()
            })
            .collect();
        let mut interner = Interner {
            palette: Vec::new(),
            index: HashMap::new(),
        };
        let mut table = vec![0u8; CODE_POINTS];
        for (code, &entry) in bin[table_start..].as_chunks::<4>().0.iter().enumerate() {
            let info = u32::from_le_bytes(entry);
            table[code] = interner.intern(Entry { info, flags: 0 }, bin_path)?;
        }
        table[BIN_TABLE_ENTRIES] = interner.intern(Entry { info: 0, flags: 0 }, bin_path)?;
        let default = table[DEFAULT_CODE];
        table[BMP_LEN..].fill(default);
        let ideograph = table[IDEOGRAPH_CODE];
        table[IDEOGRAPHIC_PLANES].fill(ideograph);
        let compatibility = table[COMPATIBILITY_IDEOGRAPH_CODE];
        table[COMPATIBILITY_IDEOGRAPHS_SUPPLEMENT].fill(compatibility);
        for code in KANA_BLOCKS {
            let is_katakana = KATAKANA_RANGES.iter().any(|range| range.contains(&code));
            table[code] = table[if is_katakana {
                KATAKANA_CODE
            } else {
                HIRAGANA_CODE
            }];
        }
        if let Some((bytes, def_path)) = def {
            let def = parse_char_def(bytes, def_path)?;
            if def.names != names {
                return Err(invalid(
                    def_path,
                    "the categories differ from those of char.bin",
                ));
            }
            for range in &def.ranges {
                let entry = Entry {
                    info: range.info,
                    flags: 0,
                };
                let entry = interner.intern(entry, bin_path)?;
                let low = (range.low as usize).max(BMP_LEN);
                if low <= range.high as usize {
                    table[low..=range.high as usize].fill(entry);
                }
            }
        }
        for code in EXTENDER_RANGES.into_iter().flatten() {
            let mut entry = interner.palette[usize::from(table[code])];
            entry.flags |= EXTENDER_FLAG;
            table[code] = interner.intern(entry, bin_path)?;
        }
        Ok(Self {
            names,
            palette: interner.palette,
            table,
        })
    }

    pub fn from_mecab(char_bin: &Path, char_def: Option<&Path>) -> Result<Self, Error> {
        let read = |path: &Path| {
            fs::read(path).map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })
        };
        let bin = read(char_bin)?;
        let def = char_def
            .map(|path| read(path).map(|bytes| (bytes, path)))
            .transpose()?;
        let def = def.as_ref().map(|(bytes, path)| (bytes.as_slice(), *path));
        Self::build(&bin, char_bin, def)
    }

    pub fn write(&self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(MAGIC)?;
        output.write_all(&VERSION.to_le_bytes())?;
        output.write_all(&(self.names.len() as u32).to_le_bytes())?;
        output.write_all(&(self.palette.len() as u32).to_le_bytes())?;
        for name in &self.names {
            let mut padded = [0u8; NAME_BYTES];
            let length = name.len().min(NAME_BYTES - 1);
            padded[..length].copy_from_slice(&name[..length]);
            output.write_all(&padded)?;
        }
        for entry in &self.palette {
            output.write_all(&entry.info.to_le_bytes())?;
            output.write_all(&entry.flags.to_le_bytes())?;
        }
        output.write_all(&self.table)
    }

    pub fn convert(char_bin: &Path, char_def: Option<&Path>, output: &Path) -> Result<(), Error> {
        let table = Self::from_mecab(char_bin, char_def)?;
        write_file(output, |writer| table.write(writer))
    }
}

pub fn migrate_char_table(dictionary: impl AsRef<Path>) -> Result<(), Error> {
    let directory = dictionary.as_ref();
    let definition = directory.join(CHAR_DEF_FILE);
    CharTable::convert(
        &directory.join(CHAR_BIN_FILE),
        definition.is_file().then_some(definition.as_path()),
        &directory.join(CHAR_TABLE_FILE),
    )
}
