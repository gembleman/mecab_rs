pub const CHAR_BIN_COUNT_BYTES: usize = 4;
pub const CHAR_BIN_ENTRY_BYTES: usize = 4;
pub const CHAR_BIN_ENTRIES: usize = 0xffff;
pub const CATEGORY_NAME_BYTES: usize = 32;
pub const MAX_CATEGORIES: usize = 18;

const TYPE_BITS: u32 = 18;
pub const TYPE_MASK: u32 = (1 << TYPE_BITS) - 1;
const DEFAULT_TYPE_BITS: u32 = 8;
pub const DEFAULT_TYPE_MASK: u32 = (1 << DEFAULT_TYPE_BITS) - 1;
pub const DEFAULT_TYPE_SHIFT: u32 = TYPE_BITS;
const LENGTH_BITS: u32 = 4;
pub const LENGTH_MASK: u32 = (1 << LENGTH_BITS) - 1;
pub const LENGTH_SHIFT: u32 = DEFAULT_TYPE_SHIFT + DEFAULT_TYPE_BITS;
pub const GROUP_SHIFT: u32 = LENGTH_SHIFT + LENGTH_BITS;
pub const INVOKE_SHIFT: u32 = GROUP_SHIFT + 1;
pub const FLAG_MASK: u32 = 1;

pub fn pack_char_info(
    type_mask: u32,
    default_type: u32,
    length: u32,
    group: u32,
    invoke: u32,
) -> u32 {
    (type_mask & TYPE_MASK)
        | (default_type & DEFAULT_TYPE_MASK) << DEFAULT_TYPE_SHIFT
        | (length & LENGTH_MASK) << LENGTH_SHIFT
        | (group & FLAG_MASK) << GROUP_SHIFT
        | (invoke & FLAG_MASK) << INVOKE_SHIFT
}
