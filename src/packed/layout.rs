pub const MAGIC: &[u8; 8] = b"MRDPACK1";
pub const VERSION: u32 = 8;
pub const FLAG_HOT_MATRIX: u32 = 2;
pub const FLAG_INT_TEMPLATES: u32 = 4;
pub const MAGIC_BYTES: usize = MAGIC.len();
pub const VERSION_AT: usize = MAGIC_BYTES;
pub const FLAGS_AT: usize = VERSION_AT + 4;
pub const CODE_COUNT_AT: usize = FLAGS_AT + 4;
pub const TOKEN_COUNT_AT: usize = CODE_COUNT_AT + 4;
pub const PARAM_COUNT_AT: usize = TOKEN_COUNT_AT + 4;
pub const CATEGORY_COUNT_AT: usize = PARAM_COUNT_AT + 4;
pub const PARAM_BITS_AT: usize = CATEGORY_COUNT_AT + 4;
pub const FEATURE_BITS_AT: usize = PARAM_BITS_AT + 4;
pub const SECTION_TABLE_AT: usize = FEATURE_BITS_AT + 4;
pub const SECTION_ENTRY_BYTES: usize = 16;
pub const SECTION_OFFSET_IN_ENTRY: usize = 0;
pub const SECTION_LENGTH_IN_ENTRY: usize = 8;
pub const SECTION_ALIGNMENT: usize = 8;
pub const HEADER_BYTES: usize = SECTION_TABLE_AT + SECTION_ENTRY_BYTES * SECTIONS;
pub const SECTIONS: usize = 12;
pub const BLOCK_CODES: usize = 256;
pub const BLOCK_COUNT: usize = 0x11_0000 / BLOCK_CODES;

pub const CHAR_INDEX: usize = 0;
pub const CHAR_BLOCKS: usize = 1;
pub const CELLS: usize = 2;
pub const TOKENS: usize = 3;
pub const PARAMS: usize = 4;
pub const FEATURES: usize = 5;
pub const UNKNOWN_STARTS: usize = 6;
pub const UNKNOWN_TOKENS: usize = 7;
pub const MATRIX: usize = 8;
pub const TEMPLATES: usize = 9;
pub const CHAR_TABLE: usize = 10;
pub const DICRC: usize = 11;

pub const UNIT_BYTES: usize = 6;
pub const VALUE_BITS: u32 = 24;
pub const VALUE_MASK: u64 = (1 << VALUE_BITS) - 1;
pub const LABEL_SHIFT: u32 = VALUE_BITS;
pub const FLAGS_SHIFT: u32 = 40;
pub const UNUSED_LABEL: u16 = u16::MAX;
pub const ROOT_LABEL: u16 = UNUSED_LABEL;
pub const MAX_CODE_COUNT: usize = UNUSED_LABEL as usize;
pub const DATA_LABEL: u16 = 0;
pub const TERMINAL_FLAG: u8 = 1;
pub const INLINE_FLAG: u8 = 2;
pub const INLINE_COUNT_SHIFT: u32 = 2;
pub const INLINE_COUNT_MASK: u8 = 0x1F;
pub const INLINE_FLAGS_END_SHIFT: u32 = INLINE_COUNT_SHIFT + INLINE_COUNT_MASK.count_ones();
pub const MAX_UNITS: usize = VALUE_MASK as usize;
pub const CELLS_PADDING: usize = 2;

pub const fn unit(value: u32, label: u16, flags: u8) -> u64 {
    value as u64 | (label as u64) << LABEL_SHIFT | (flags as u64) << FLAGS_SHIFT
}

#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Unit(u64);

impl Unit {
    #[inline(always)]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    #[inline(always)]
    pub const fn value(self) -> u32 {
        (self.0 & VALUE_MASK) as u32
    }

    #[inline(always)]
    pub const fn label(self) -> u16 {
        (self.0 >> LABEL_SHIFT) as u16
    }

    #[inline(always)]
    pub const fn flags(self) -> u8 {
        (self.0 >> FLAGS_SHIFT) as u8
    }

    #[inline(always)]
    pub const fn is_terminal(self) -> bool {
        self.flags() & TERMINAL_FLAG != 0
    }

    #[inline(always)]
    pub const fn is_inline(self) -> bool {
        self.flags() & INLINE_FLAG != 0
    }

    #[inline(always)]
    pub const fn inline_count(self) -> u8 {
        (self.flags() >> INLINE_COUNT_SHIFT) & INLINE_COUNT_MASK
    }

    #[inline(always)]
    pub const fn has_unused_flag_bits(self) -> bool {
        self.flags() >> INLINE_FLAGS_END_SHIFT != 0
    }
}
