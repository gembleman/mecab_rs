pub mod char_layout {
    pub use crate::char_layout::*;
}

pub mod dictionary_format {
    pub use crate::system_dictionary::{
        CHARSET_BYTES, DIC_VERSION, DictionaryKind, HEADER_SIZE, MAGIC_ID, PrefixHit,
        SystemDictionary, TOKEN_SIZE, VALUE_SIZE_BITS,
    };
}

pub mod files {
    pub use crate::dict_files::*;
    pub use crate::error::io_error;
    pub use crate::file_write::{write_file, write_file_with};
}

pub mod hash {
    pub use crate::wire::{FNV_OFFSET, FNV_PRIME};
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
