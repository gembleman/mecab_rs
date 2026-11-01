const INLINE_ENTRY_COUNT: usize = 2;
const FIRST_ARENA_CAP: u32 = 4;

#[derive(Clone, Copy, Default)]
pub(super) struct Table {
    pub(super) total: u32,
    pub(super) len: u32,
    pub(super) cap: u32,
    pub(super) start: u32,
    inline_entries: [(u32, u32); INLINE_ENTRY_COUNT],
}

impl Table {
    #[inline]
    pub(super) fn entries<'a>(&'a self, arena: &'a [(u32, u32)]) -> &'a [(u32, u32)] {
        if self.cap == 0 {
            &self.inline_entries[..self.len as usize]
        } else {
            &arena[self.start as usize..(self.start + self.len) as usize]
        }
    }

    #[inline]
    pub(super) fn entries_mut<'a>(
        &'a mut self,
        arena: &'a mut [(u32, u32)],
    ) -> &'a mut [(u32, u32)] {
        if self.cap == 0 {
            &mut self.inline_entries[..self.len as usize]
        } else {
            &mut arena[self.start as usize..(self.start + self.len) as usize]
        }
    }

    pub(super) fn push(&mut self, arena: &mut Vec<(u32, u32)>, e: (u32, u32)) {
        if self.cap == 0 {
            if (self.len as usize) < INLINE_ENTRY_COUNT {
                self.inline_entries[self.len as usize] = e;
                self.len += 1;
                return;
            }
            self.start = arena.len() as u32;
            self.cap = FIRST_ARENA_CAP;
            arena.extend_from_slice(&self.inline_entries);
            arena.extend_from_slice(&[(0, 0), (0, 0)]);
        } else if self.len == self.cap {
            let new_start = arena.len() as u32;
            let old = self.start as usize;
            arena.extend_from_within(old..old + self.len as usize);
            self.start = new_start;
            self.cap *= 2;
            arena.resize(new_start as usize + self.cap as usize, (0, 0));
        }
        arena[(self.start + self.len) as usize] = e;
        self.len += 1;
    }
}
