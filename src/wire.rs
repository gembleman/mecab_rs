pub const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
#[cfg(feature = "xz")]
const VARINT_PAYLOAD_BITS: u32 = 7;
#[cfg(feature = "xz")]
const VARINT_PAYLOAD_MASK: u8 = 0x7F;
#[cfg(feature = "xz")]
const VARINT_CONTINUE: u8 = 0x80;
#[cfg(feature = "xz")]
const VARINT_MAX_SHIFT: u32 = 63;

pub(crate) fn fnv1a(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |state, &byte| {
        (state ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

#[cfg(feature = "xz")]
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a(FNV_OFFSET, bytes)
}

#[cfg(feature = "xz")]
pub(crate) fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= u64::from(VARINT_CONTINUE) {
        out.push((value as u8 & VARINT_PAYLOAD_MASK) | VARINT_CONTINUE);
        value >>= VARINT_PAYLOAD_BITS;
    }
    out.push(value as u8);
}

#[cfg(feature = "xz")]
pub(crate) fn get_varint(bytes: &[u8], position: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*position)?;
        *position += 1;
        if shift > VARINT_MAX_SHIFT {
            return None;
        }
        value |= u64::from(byte & VARINT_PAYLOAD_MASK) << shift;
        if byte & VARINT_CONTINUE == 0 {
            return Some(value);
        }
        shift += VARINT_PAYLOAD_BITS;
    }
}

#[cfg(feature = "xz")]
pub(crate) fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

#[cfg(feature = "xz")]
pub(crate) fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[inline]
pub(crate) fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}

#[inline]
pub fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}

#[inline]
pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}
