use std::ops::{Deref, Range};
use std::sync::Arc;

use memmap2::Mmap;

#[derive(Clone)]
pub(crate) struct MappedBytes {
    map: Arc<Mmap>,
    range: Range<usize>,
}

impl MappedBytes {
    pub(crate) fn whole(map: Mmap) -> Self {
        let length = map.len();
        Self {
            map: Arc::new(map),
            range: 0..length,
        }
    }

    pub(crate) fn from_arc(map: Arc<Mmap>, range: Range<usize>) -> Option<Self> {
        (range.start <= range.end && range.end <= map.len()).then_some(Self { map, range })
    }

    pub(crate) fn slice(&self, range: Range<usize>) -> Option<&[u8]> {
        let start = self.range.start.checked_add(range.start)?;
        let end = self.range.start.checked_add(range.end)?;
        (end <= self.range.end)
            .then(|| self.map.get(start..end).expect("mapped range is in bounds"))
    }
}

impl Deref for MappedBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.map[self.range.clone()]
    }
}
