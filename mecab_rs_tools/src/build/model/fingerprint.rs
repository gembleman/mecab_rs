use mecab_rs::__private::in_range_bytes::u32_at;

const FINGERPRINT_SEED: u32 = 0xfd14_deff;

fn rotl(value: u32, shift: u32) -> u32 {
    value.rotate_left(shift)
}

fn fmix(mut hash: u32) -> u32 {
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^ (hash >> 16)
}

pub(super) fn fingerprint(key: &[u8]) -> u64 {
    const C1: u32 = 0x239b_961b;
    const C2: u32 = 0xab0e_9789;
    const C3: u32 = 0x38b3_4ae5;
    const C4: u32 = 0xa1e3_8b93;
    let (mut h1, mut h2, mut h3, mut h4) = (
        FINGERPRINT_SEED,
        FINGERPRINT_SEED,
        FINGERPRINT_SEED,
        FINGERPRINT_SEED,
    );
    let (blocks, tail) = key.as_chunks::<16>();
    for block in blocks {
        let word = |index: usize| u32_at(block, index * 4);
        let (mut k1, mut k2, mut k3, mut k4) = (word(0), word(1), word(2), word(3));
        k1 = rotl(k1.wrapping_mul(C1), 15).wrapping_mul(C2);
        h1 ^= k1;
        h1 = rotl(h1, 19)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x561c_cd1b);
        k2 = rotl(k2.wrapping_mul(C2), 16).wrapping_mul(C3);
        h2 ^= k2;
        h2 = rotl(h2, 17)
            .wrapping_add(h3)
            .wrapping_mul(5)
            .wrapping_add(0x0bca_a747);
        k3 = rotl(k3.wrapping_mul(C3), 17).wrapping_mul(C4);
        h3 ^= k3;
        h3 = rotl(h3, 15)
            .wrapping_add(h4)
            .wrapping_mul(5)
            .wrapping_add(0x96cd_1c35);
        k4 = rotl(k4.wrapping_mul(C4), 18).wrapping_mul(C1);
        h4 ^= k4;
        h4 = rotl(h4, 13)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x32ac_3b17);
    }
    let mut ks = [0u32; 4];
    for (index, &byte) in tail.iter().enumerate() {
        ks[index / 4] ^= u32::from(byte) << (8 * (index % 4));
    }
    if tail.len() > 12 {
        h4 ^= rotl(ks[3].wrapping_mul(C4), 18).wrapping_mul(C1);
    }
    if tail.len() > 8 {
        h3 ^= rotl(ks[2].wrapping_mul(C3), 17).wrapping_mul(C4);
    }
    if tail.len() > 4 {
        h2 ^= rotl(ks[1].wrapping_mul(C2), 16).wrapping_mul(C3);
    }
    if !tail.is_empty() {
        h1 ^= rotl(ks[0].wrapping_mul(C1), 15).wrapping_mul(C2);
    }
    let length = key.len() as u32;
    h1 ^= length;
    h2 ^= length;
    h3 ^= length;
    h4 ^= length;
    h1 = h1.wrapping_add(h2).wrapping_add(h3).wrapping_add(h4);
    h2 = h2.wrapping_add(h1);
    h3 = h3.wrapping_add(h1);
    h4 = h4.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h3 = fmix(h3);
    h4 = fmix(h4);
    h1 = h1.wrapping_add(h2).wrapping_add(h3).wrapping_add(h4);
    h2 = h2.wrapping_add(h1);
    u64::from(h1) | u64::from(h2) << 32
}
