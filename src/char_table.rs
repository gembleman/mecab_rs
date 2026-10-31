use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::path::Path;

use crate::char_layout::{
    CATEGORY_NAME_BYTES, CHAR_BIN_COUNT_BYTES, CHAR_BIN_ENTRIES, CHAR_BIN_ENTRY_BYTES,
    MAX_CATEGORIES, pack_char_info,
};
use crate::error::{Error, IoAction, io_error};
use crate::file_write::write_file;

pub(crate) const MAGIC: &[u8; 4] = b"MXCU";
pub(crate) const VERSION: u32 = 1;
pub(crate) const CHAR_TABLE_HEADER_BYTES: usize = 16;
pub(crate) const PALETTE_ENTRY_BYTES: usize = 8;
pub(crate) const MAX_PALETTE: usize = 256;
pub(crate) const CODE_POINTS: usize = 0x11_0000;
pub(crate) const BMP_LEN: usize = 0x1_0000;
pub(crate) const EXTENDER_FLAG: u32 = 1;

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

impl fmt::Debug for CharTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CharTable")
            .field("categories", &self.names.len())
            .field("palette_entries", &self.palette.len())
            .field("table_bytes", &self.table.len())
            .finish_non_exhaustive()
    }
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

#[derive(Clone, Copy)]
struct Category {
    id: u32,
    invoke: u32,
    group: u32,
    length: u32,
}

struct CharDefParser<'a> {
    file: &'a Path,
    categories: HashMap<&'a [u8], Category>,
    names: Vec<Vec<u8>>,
    ranges: Vec<DefRange>,
}

impl<'a> CharDefParser<'a> {
    fn new(file: &'a Path) -> Self {
        Self {
            file,
            categories: HashMap::new(),
            names: Vec::new(),
            ranges: Vec::new(),
        }
    }

    fn fail(&self, line: usize, reason: impl Into<Cow<'static, str>>) -> Error {
        Error::format_at(self.file, line, reason)
    }

    fn read_line(&mut self, number: usize, raw: &'a [u8]) -> Result<(), Error> {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() || line[0] == b'#' {
            return Ok(());
        }
        let columns: Vec<&[u8]> = line
            .split(|&byte| byte == b'\t' || byte == b' ')
            .filter(|column| !column.is_empty())
            .collect();
        if columns.len() < 2 {
            return Err(self.fail(number, "format error"));
        }
        if columns[0].starts_with(b"0x") {
            self.read_range(number, &columns)
        } else {
            self.read_category(number, &columns)
        }
    }

    fn read_range(&mut self, number: usize, columns: &[&[u8]]) -> Result<(), Error> {
        let (low, high) = match columns[0].windows(2).position(|pair| pair == b"..") {
            Some(at) => (&columns[0][..at], &columns[0][at + 2..]),
            None => (columns[0], columns[0]),
        };
        let low = atohex(low).map_err(|reason| self.fail(number, reason))?;
        let high = atohex(high).map_err(|reason| self.fail(number, reason))?;
        if low > high {
            return Err(self.fail(number, format!("range error: low={low} high={high}")));
        }
        let chosen = self.chosen_categories(number, &columns[1..])?;
        let Some(first) = chosen.first() else {
            return Err(self.fail(number, "category size is empty"));
        };
        let type_mask = chosen
            .iter()
            .fold(0u32, |mask, category| mask.wrapping_add(1 << category.id));
        self.ranges.push(DefRange {
            low,
            high,
            info: pack_char_info(type_mask, first.id, first.length, first.group, first.invoke),
        });
        Ok(())
    }

    fn chosen_categories(&self, number: usize, columns: &[&[u8]]) -> Result<Vec<Category>, Error> {
        let mut chosen = Vec::new();
        for &column in columns.iter().take_while(|column| column[0] != b'#') {
            let Some(&category) = self.categories.get(column) else {
                let name = String::from_utf8_lossy(column);
                return Err(self.fail(number, format!("category [{name}] is undefined")));
            };
            chosen.push(category);
        }
        Ok(chosen)
    }

    fn read_category(&mut self, number: usize, columns: &[&'a [u8]]) -> Result<(), Error> {
        if columns.len() < 4 {
            return Err(self.fail(number, "format error"));
        }
        if self.categories.contains_key(columns[0]) {
            return Err(self.fail(number, "category is already defined"));
        }
        let category = Category {
            id: self.names.len() as u32,
            invoke: atoi(columns[1]),
            group: atoi(columns[2]),
            length: atoi(columns[3]),
        };
        self.categories.insert(columns[0], category);
        self.names.push(columns[0].to_vec());
        Ok(())
    }

    fn finish(self) -> Result<CharDef, Error> {
        if self.names.len() >= MAX_CATEGORIES {
            return Err(Error::format(self.file, "too many categories(>= 18)"));
        }
        for required in [&b"DEFAULT"[..], b"SPACE"] {
            if !self.categories.contains_key(required) {
                let name = String::from_utf8_lossy(required);
                return Err(Error::format(
                    self.file,
                    format!("category [{name}] is undefined"),
                ));
            }
        }
        Ok(CharDef {
            names: self.names,
            ranges: self.ranges,
        })
    }
}

fn parse_char_def(bytes: &[u8], file: &Path) -> Result<CharDef, Error> {
    let mut parser = CharDefParser::new(file);
    for (index, raw) in bytes.split(|&byte| byte == b'\n').enumerate() {
        parser.read_line(index + 1, raw)?;
    }
    parser.finish()
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
            .map_err(|_| Error::format(file, "more than 256 distinct character entries"))?;
        self.palette.push(entry);
        self.index.insert(entry, index);
        Ok(index)
    }
}

fn read_names(bin: &[u8], bin_path: &Path) -> Result<(Vec<Vec<u8>>, usize), Error> {
    let broken = |reason| Error::format(bin_path, reason);
    let Some(&count) = bin.first_chunk() else {
        return Err(broken("the file is shorter than its header"));
    };
    let categories = u32::from_le_bytes(count) as usize;
    let table_start = categories
        .checked_mul(CATEGORY_NAME_BYTES)
        .and_then(|names| names.checked_add(CHAR_BIN_COUNT_BYTES))
        .ok_or_else(|| broken("the file size does not match the header"))?;
    if table_start.checked_add(CHAR_BIN_ENTRY_BYTES * CHAR_BIN_ENTRIES) != Some(bin.len()) {
        return Err(broken("the file size does not match the header"));
    }
    if categories == 0 {
        return Err(broken("the file has no character categories"));
    }
    let names: Vec<Vec<u8>> = bin[CHAR_BIN_COUNT_BYTES..table_start]
        .as_chunks::<CATEGORY_NAME_BYTES>()
        .0
        .iter()
        .map(|name| {
            let length = name.iter().position(|&byte| byte == 0);
            name[..length.unwrap_or(CATEGORY_NAME_BYTES)].to_vec()
        })
        .collect();
    if names.iter().any(|name| name.len() >= CATEGORY_NAME_BYTES) {
        return Err(broken("a category name has no terminating NUL"));
    }
    Ok((names, table_start))
}

fn fill_bin_table(
    entries: &[u8],
    interner: &mut Interner,
    bin_path: &Path,
) -> Result<Vec<u8>, Error> {
    let mut table = vec![0u8; CODE_POINTS];
    for (code, &entry) in entries
        .as_chunks::<CHAR_BIN_ENTRY_BYTES>()
        .0
        .iter()
        .enumerate()
    {
        let info = u32::from_le_bytes(entry);
        table[code] = interner.intern(Entry { info, flags: 0 }, bin_path)?;
    }
    table[CHAR_BIN_ENTRIES] = interner.intern(Entry { info: 0, flags: 0 }, bin_path)?;
    Ok(table)
}

fn fill_unicode_defaults(table: &mut [u8]) {
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
}

fn apply_char_def(
    table: &mut [u8],
    interner: &mut Interner,
    names: &[Vec<u8>],
    (bytes, def_path): (&[u8], &Path),
    bin_path: &Path,
) -> Result<(), Error> {
    let def = parse_char_def(bytes, def_path)?;
    if def.names != names {
        return Err(Error::format(
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
    Ok(())
}

fn mark_extenders(table: &mut [u8], interner: &mut Interner, bin_path: &Path) -> Result<(), Error> {
    for code in EXTENDER_RANGES.into_iter().flatten() {
        let mut entry = interner.palette[usize::from(table[code])];
        entry.flags |= EXTENDER_FLAG;
        table[code] = interner.intern(entry, bin_path)?;
    }
    Ok(())
}

impl CharTable {
    fn build(bin: &[u8], bin_path: &Path, def: Option<(&[u8], &Path)>) -> Result<Self, Error> {
        let (names, table_start) = read_names(bin, bin_path)?;
        let mut interner = Interner {
            palette: Vec::new(),
            index: HashMap::new(),
        };
        let mut table = fill_bin_table(&bin[table_start..], &mut interner, bin_path)?;
        fill_unicode_defaults(&mut table);
        if let Some(def) = def {
            apply_char_def(&mut table, &mut interner, &names, def, bin_path)?;
        }
        mark_extenders(&mut table, &mut interner, bin_path)?;
        Ok(Self {
            names,
            palette: interner.palette,
            table,
        })
    }

    pub fn from_mecab(char_bin: &Path, char_def: Option<&Path>) -> Result<Self, Error> {
        let read = |path: &Path| fs::read(path).map_err(io_error(IoAction::Read, path));
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
            let mut padded = [0u8; CATEGORY_NAME_BYTES];
            debug_assert!(name.len() < CATEGORY_NAME_BYTES);
            padded[..name.len()].copy_from_slice(name);
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
