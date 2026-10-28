#[cfg(feature = "xz")]
use std::io::Write;
#[cfg(feature = "xz")]
use std::path::Path;

#[cfg(feature = "xz")]
use lzma_rust2::{XzOptions, XzWriter};

#[cfg(feature = "xz")]
use crate::error::Error;

pub(crate) const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
pub(crate) const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const VARINT_PAYLOAD_BITS: u32 = 7;
const VARINT_PAYLOAD_MASK: u8 = 0x7F;
const VARINT_CONTINUE: u8 = 0x80;
const VARINT_MAX_SHIFT: u32 = 63;
#[cfg(feature = "xz")]
const XZ_PRESET: u32 = 9;
#[cfg(feature = "xz")]
const XZ_NICE_LEN: u32 = 273;
#[cfg(feature = "xz")]
const XZ_DEPTH_LIMIT: i32 = 512;

pub(crate) fn fnv1a(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |state, &byte| {
        (state ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a(FNV_OFFSET, bytes)
}

pub(crate) fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= u64::from(VARINT_CONTINUE) {
        out.push((value as u8 & VARINT_PAYLOAD_MASK) | VARINT_CONTINUE);
        value >>= VARINT_PAYLOAD_BITS;
    }
    out.push(value as u8);
}

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

pub(crate) fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

pub(crate) fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[inline]
pub(crate) fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}

#[inline]
pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}

#[inline]
pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(*bytes[at..].first_chunk().expect("in range"))
}

#[cfg(feature = "xz")]
pub(crate) fn compress_xz(
    payload: &[u8],
    path: &Path,
    largest_dictionary: u32,
) -> Result<Vec<u8>, Error> {
    let mut options = XzOptions::with_preset(XZ_PRESET);
    options.lzma_options.nice_len = XZ_NICE_LEN;
    options.lzma_options.depth_limit = XZ_DEPTH_LIMIT;
    options.lzma_options.dict_size = options
        .lzma_options
        .dict_size
        .max(u32::try_from(payload.len()).unwrap_or(u32::MAX))
        .min(largest_dictionary);
    let failed = |error: std::io::Error| Error::Rebuild {
        path: path.to_path_buf(),
        reason: format!("xz: {error}"),
    };
    let mut writer = XzWriter::new(Vec::new(), options).map_err(failed)?;
    writer.write_all(payload).map_err(failed)?;
    writer.finish().map_err(failed)
}
