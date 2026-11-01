pub mod char_layout {
    pub use crate::char_layout::*;
}

pub mod dictionary_format {
    pub use crate::system_dictionary::{
        CHARSET_BYTES, DIC_VERSION, DictionaryKind, HEADER_SIZE, MAGIC_ID, PrefixHit,
        SystemDictionary, TOKEN_SIZE, VALUE_SIZE_BITS,
    };
}

pub mod errors {
    use std::io;
    use std::path::Path;

    use crate::{Error, ToolStage};

    pub use crate::error::{IoAction, Undecodable};

    pub fn io(action: IoAction, path: &Path, source: io::Error) -> Error {
        Error::io(action, path, source)
    }

    pub fn io_error(action: IoAction, path: &Path) -> impl Fn(io::Error) -> Error + '_ {
        crate::error::io_error(action, path)
    }

    pub fn output(source: io::Error) -> Error {
        Error::output(source)
    }

    pub fn tool(stage: ToolStage, path: &Path, reason: impl Into<String>) -> Error {
        Error::tool(stage, path, reason)
    }

    pub fn on_line(error: Error, line: usize) -> Error {
        error.on_line(line)
    }

    pub fn caused_by(
        error: Error,
        cause: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Error {
        error.caused_by(cause)
    }
}

pub mod files {
    pub use crate::dict_files::*;
    pub use crate::expand_cache::cache_path as expanded_cache_path;
    pub use crate::file_write::{write_file, write_file_with};
}

pub mod hash {
    pub use crate::hash::{FastMap, FastSet};
}

pub mod in_range_bytes {
    pub use crate::wire::{i16_at, u16_at, u32_at, u32_values, u64_at};
}

pub mod matrix {
    pub use crate::hot_shape::HotShape;
    pub use crate::int_runtime::RuntimeIntTemplates;
    pub use crate::int_templates::{IntTemplates, MAX_COST_FACTOR, MappedIntTemplates};
    pub use crate::matrix::{MATRIX_COST_BYTES, MATRIX_HEADER_BYTES};
    pub use crate::part::{IntPart, Part};
}

pub mod support {
    pub use crate::threads::default_threads;

    #[cfg(feature = "xz")]
    pub use crate::packed::transport::transport_payload;
    #[cfg(feature = "pair-stats")]
    pub use crate::pair_stats::take as take_pair_counts;
}

pub mod template_universe {
    pub use crate::template_universe::{id_rows, parse_side, project, read_bigrams, side_keys};
}
