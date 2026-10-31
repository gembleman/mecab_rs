use super::access;
use crate::packed::bits::extract;

#[inline(always)]
pub(in crate::packed) fn read_bits(bytes: &[u8], bit: usize, width: u32) -> u64 {
    if width <= 56 {
        read_narrow(bytes, bit, width)
    } else {
        read_narrow(bytes, bit, 32) | read_narrow(bytes, bit + 32, width - 32) << 32
    }
}

#[inline(always)]
pub(in crate::packed) fn read_small<const TRUSTED: bool>(
    bytes: &[u8],
    bit: usize,
    width: u32,
) -> u64 {
    extract(access::word::<TRUSTED>(bytes, bit >> 3), bit, width)
}

#[inline(always)]
fn read_narrow(bytes: &[u8], bit: usize, width: u32) -> u64 {
    extract(access::word::<false>(bytes, bit >> 3), bit, width)
}
