const MIX_HASH_MULTIPLIER: u64 = 0xFF51_AFD7_ED55_8CCD;
const MIX_VALUE_MULTIPLIER: u64 = 0xC4CE_B9FE_1A85_EC53;
const MIX_INCREMENT: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX_FIRST_ROUND_MULTIPLIER: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX_SECOND_ROUND_MULTIPLIER: u64 = 0x94D0_49BB_1331_11EB;
const HASH_BYTES_SEED: u64 = 0x243F_6A88_85A3_08D3;

#[inline]
pub(crate) fn mix(h: u64, x: u64) -> u64 {
    let mut z = h
        .wrapping_mul(MIX_HASH_MULTIPLIER)
        .wrapping_add(x.wrapping_mul(MIX_VALUE_MULTIPLIER))
        .wrapping_add(MIX_INCREMENT);
    z = (z ^ (z >> 30)).wrapping_mul(MIX_FIRST_ROUND_MULTIPLIER);
    z = (z ^ (z >> 27)).wrapping_mul(MIX_SECOND_ROUND_MULTIPLIER);
    z ^ (z >> 31)
}

pub(crate) fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = HASH_BYTES_SEED ^ b.len() as u64;
    let (chunks, rest) = b.as_chunks::<8>();
    for &ch in chunks {
        h = mix(h, u64::from_le_bytes(ch));
    }
    if !rest.is_empty() {
        let mut buf = [0u8; 8];
        buf[..rest.len()].copy_from_slice(rest);
        h = mix(h, u64::from_le_bytes(buf));
    }
    h
}
