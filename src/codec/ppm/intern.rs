use super::hash::{FastMap, hash_bytes};

#[derive(Default)]
pub(crate) struct Interner {
    map: FastMap<u64, u32>,
    buf: Vec<u8>,
    ends: Vec<usize>,
}

impl Interner {
    pub(crate) fn get(&self, id: u32) -> &[u8] {
        let i = id as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.buf[start..self.ends[i]]
    }

    pub(crate) fn find(&self, key: &[u8]) -> Option<u32> {
        let mut h = hash_bytes(key);
        loop {
            match self.map.get(&h) {
                None => return None,
                Some(&id) if self.get(id) == key => return Some(id),
                Some(_) => h = h.wrapping_add(1),
            }
        }
    }

    pub(crate) fn add(&mut self, key: &[u8]) -> u32 {
        let mut h = hash_bytes(key);
        while self.map.contains_key(&h) {
            h = h.wrapping_add(1);
        }
        let id = self.ends.len() as u32;
        self.buf.extend_from_slice(key);
        self.ends.push(self.buf.len());
        self.map.insert(h, id);
        id
    }

    #[inline]
    pub(crate) fn find_or_add(&mut self, key: &[u8]) -> u32 {
        match self.find(key) {
            Some(id) => id,
            None => self.add(key),
        }
    }
}
