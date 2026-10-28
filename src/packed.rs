mod bits;
mod features;
mod model;
mod order;
mod reader;
mod source;
#[cfg(feature = "xz")]
mod transport;
mod trie;

use std::path::Path;

use crate::Error;
use crate::build::write_file;

pub use reader::{Hit, PackedDictionary, PackedView, TokenRecord};
#[cfg(feature = "xz")]
pub use transport::{expand, write_transport};

pub(crate) const MAGIC: &[u8; 8] = b"MRDPACK1";
pub(crate) const VERSION: u32 = 5;
pub(crate) const FLAG_HOT_MATRIX: u32 = 2;
pub(crate) const FLAG_INT_TEMPLATES: u32 = 4;
pub(crate) const HEADER_BYTES: usize = 40 + 16 * SECTIONS;
pub(crate) const SECTIONS: usize = 12;
pub(crate) const BLOCK_CODES: usize = 256;
pub(crate) const BLOCK_COUNT: usize = 0x11_0000 / BLOCK_CODES;

pub(crate) const CHAR_INDEX: usize = 0;
pub(crate) const CHAR_BLOCKS: usize = 1;
pub(crate) const CELLS: usize = 2;
pub(crate) const TOKENS: usize = 3;
pub(crate) const PARAMS: usize = 4;
pub(crate) const FEATURES: usize = 5;
pub(crate) const UNKNOWN_STARTS: usize = 6;
pub(crate) const UNKNOWN_TOKENS: usize = 7;
pub(crate) const MATRIX: usize = 8;
pub(crate) const TEMPLATES: usize = 9;
pub(crate) const CHAR_TABLE: usize = 10;
pub(crate) const DICRC: usize = 11;

pub const SECTION_NAMES: [&str; SECTIONS] = [
    "char index",
    "char blocks",
    "double array",
    "tokens",
    "params",
    "features",
    "unknown starts",
    "unknown tokens",
    "matrix",
    "templates",
    "char table",
    "dicrc",
];

pub(crate) const UNIT_BYTES: usize = 6;
pub(crate) const VALUE_MASK: u64 = 0x00FF_FFFF;
pub(crate) const LABEL_SHIFT: u32 = 24;
pub(crate) const FLAGS_SHIFT: u32 = 40;
pub(crate) const UNUSED_LABEL: u16 = u16::MAX;
pub(crate) const ROOT_LABEL: u16 = UNUSED_LABEL;
pub(crate) const TERMINAL_FLAG: u8 = 1;
pub(crate) const INLINE_FLAG: u8 = 2;
pub(crate) const INLINE_COUNT_SHIFT: u32 = 2;
pub(crate) const INLINE_COUNT_MASK: u8 = 0x1F;
pub(crate) const MAX_UNITS: usize = VALUE_MASK as usize;
pub(crate) const CELLS_PADDING: usize = 2;

pub(crate) const fn unit(value: u32, label: u16, flags: u8) -> u64 {
    value as u64 | (label as u64) << LABEL_SHIFT | (flags as u64) << FLAGS_SHIFT
}

pub(crate) const fn label_of(unit: u64) -> u16 {
    (unit >> LABEL_SHIFT) as u16
}

pub fn write_runtime(dict_dir: &Path, out_file: &Path) -> Result<(), Error> {
    let model = source::read_dictionary(dict_dir)?;
    let runtime = model.encode_runtime()?;
    write_file(out_file, |writer| runtime.write_to(writer))
}
