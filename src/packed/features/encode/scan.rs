use super::{
    BuildHasherDefault, FINAL_MULTIPLIER, HASH_MULTIPLIER, HashMap, HashSet, Hasher, MAX_FIELDS,
};

#[derive(Default)]
pub(super) struct SmallKeyHasher(u64);

impl SmallKeyHasher {
    pub(super) fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(HASH_MULTIPLIER);
    }
}

impl Hasher for SmallKeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for &chunk in chunks {
            self.add(u64::from_le_bytes(chunk));
        }
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        self.add(u64::from_le_bytes(last) ^ (rest.len() as u64) << 59);
    }

    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    fn write_u16(&mut self, value: u16) {
        self.add(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    fn finish(&self) -> u64 {
        let mut hash = self.0;
        hash ^= hash >> 32;
        hash = hash.wrapping_mul(FINAL_MULTIPLIER);
        hash ^ (hash >> 29)
    }
}

pub(super) type SmallKeyMap<K, V> = HashMap<K, V, BuildHasherDefault<SmallKeyHasher>>;

pub(super) type SmallKeySet<K> = HashSet<K, BuildHasherDefault<SmallKeyHasher>>;

pub(super) fn split_fields(text: &[u8], fields: &mut Vec<(usize, usize)>) {
    fields.clear();
    let length = text.len();
    let mut start = 0;
    loop {
        if fields.len() == MAX_FIELDS - 1 {
            fields.push((start, length));
            return;
        }
        let mut end = None;
        if text.get(start) == Some(&b'"') {
            let mut at = start + 1;
            while let Some(found) = text[at..].iter().position(|&byte| byte == b'"') {
                let quote = at + found;
                if text.get(quote + 1) == Some(&b'"') {
                    at = quote + 2;
                    continue;
                }
                if quote + 1 == length || text[quote + 1] == b',' {
                    end = Some(quote + 1);
                }
                break;
            }
        }
        let end = end.unwrap_or_else(|| {
            text[start..]
                .iter()
                .position(|&byte| byte == b',')
                .map_or(length, |found| start + found)
        });
        fields.push((start, end));
        if end == length {
            return;
        }
        start = end + 1;
    }
}

pub(super) fn parse_number(field: &[u8]) -> Option<u64> {
    if field.is_empty() || field.len() > 20 || (field[0] == b'0' && field.len() > 1) {
        return None;
    }
    let mut value = 0u64;
    for &byte in field {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}
