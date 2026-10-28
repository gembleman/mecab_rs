pub(crate) mod bits;
pub(crate) mod features;
pub(crate) mod layout;
pub(crate) mod model;
pub(crate) mod order;
pub(crate) mod reader;
pub(crate) mod source;
#[cfg(feature = "xz")]
pub(crate) mod transport;
pub(crate) mod trie;

use std::path::Path;

use crate::Error;
use crate::file_write::write_file;

pub(crate) use layout::*;
pub(crate) use reader::{PackedDictionary, PackedView};
#[cfg(feature = "xz")]
pub use transport::expand;

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

pub fn write_runtime(dict_dir: &Path, out_file: &Path) -> Result<(), Error> {
    let model = source::read_dictionary(dict_dir)?;
    let runtime = model.encode_runtime()?;
    write_file(out_file, |writer| runtime.write_to(writer))
}
