use std::hash::BuildHasherDefault;

use super::hash::hash_bytes;
use crate::hash::IdMap;

#[derive(Default)]
pub(crate) struct Interner {
    map: IdMap<u64, u32>,
    buf: Vec<u8>,
    ends: Vec<u32>,
}

impl Interner {
    pub(crate) fn with_sizes(count: usize, bytes: usize) -> Self {
        Interner {
            map: IdMap::with_capacity_and_hasher(count, BuildHasherDefault::default()),
            buf: Vec::with_capacity(bytes),
            ends: Vec::with_capacity(count),
        }
    }

    pub(crate) fn sizes(&self, out: &mut Vec<u32>) {
        out.push(self.ends.len() as u32);
        out.push(self.buf.len() as u32);
    }

    pub(crate) fn get(&self, id: u32) -> &[u8] {
        let i = id as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.buf[start..self.ends[i] as usize]
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
        debug_assert!(u32::try_from(self.buf.len()).is_ok());
        self.ends.push(self.buf.len() as u32);
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
