use super::{BIG_AT, MAX_SYMBOLS, TOTAL_LIMIT};

const INLINE_ENTRY_COUNT: usize = 2;
const FIRST_ARENA_CAP: u32 = 4;
const MAX_TOTAL_BEFORE_HALVING: u32 = TOTAL_LIMIT + 2;

const _: () = assert!(MAX_TOTAL_BEFORE_HALVING <= u16::MAX as u32);
const _: () = assert!(MAX_SYMBOLS < u16::MAX as u32);
const _: () = assert!(FIRST_ARENA_CAP as usize > INLINE_ENTRY_COUNT);
const _: () = assert!(FIRST_ARENA_CAP.is_power_of_two());
const _: () = assert!(BIG_AT >= FIRST_ARENA_CAP);

#[derive(Clone, Copy, Default)]
pub(super) struct Table {
    total: u16,
    len: u16,
    start: u32,
    inline_entries: [(u32, u32); INLINE_ENTRY_COUNT],
}

const _: () = assert!(size_of::<Table>() == 24);

impl Table {
    #[inline]
    pub(super) fn total(&self) -> u32 {
        u32::from(self.total)
    }

    #[inline]
    pub(super) fn len(&self) -> u32 {
        u32::from(self.len)
    }

    #[inline]
    pub(super) fn set_total(&mut self, total: u32) {
        debug_assert!(total <= MAX_TOTAL_BEFORE_HALVING);
        self.total = total as u16;
    }

    #[inline]
    pub(super) fn add_total(&mut self, inc: u32) {
        self.set_total(self.total() + inc);
    }

    #[inline]
    pub(super) fn count_one_more(&mut self) {
        debug_assert!(self.len() < MAX_SYMBOLS);
        self.len += 1;
    }

    #[inline]
    pub(super) fn is_big(&self) -> bool {
        self.len() > BIG_AT
    }

    #[inline]
    pub(super) fn big_index(&self) -> usize {
        debug_assert!(self.is_big());
        self.start as usize
    }

    #[inline]
    pub(super) fn set_big_index(&mut self, index: usize) {
        self.start = index as u32;
    }

    #[inline]
    fn arena_cap(&self) -> u32 {
        debug_assert!(self.len() as usize > INLINE_ENTRY_COUNT && !self.is_big());
        FIRST_ARENA_CAP.max(self.len().next_power_of_two())
    }

    #[inline]
    pub(super) fn entries<'a>(&'a self, arena: &'a [(u32, u32)]) -> &'a [(u32, u32)] {
        if self.len() as usize <= INLINE_ENTRY_COUNT {
            &self.inline_entries[..self.len as usize]
        } else {
            &arena[self.start as usize..(self.start + self.len()) as usize]
        }
    }

    #[inline]
    pub(super) fn entries_mut<'a>(
        &'a mut self,
        arena: &'a mut [(u32, u32)],
    ) -> &'a mut [(u32, u32)] {
        if self.len() as usize <= INLINE_ENTRY_COUNT {
            &mut self.inline_entries[..self.len as usize]
        } else {
            let start = self.start as usize;
            &mut arena[start..start + self.len as usize]
        }
    }

    pub(super) fn push(&mut self, arena: &mut Vec<(u32, u32)>, e: (u32, u32)) {
        debug_assert!(!self.is_big());
        let len = self.len();
        if (len as usize) < INLINE_ENTRY_COUNT {
            self.inline_entries[len as usize] = e;
            self.len += 1;
            return;
        }
        if len as usize == INLINE_ENTRY_COUNT {
            self.start = arena.len() as u32;
            arena.extend_from_slice(&self.inline_entries);
            arena.resize(self.start as usize + FIRST_ARENA_CAP as usize, (0, 0));
        } else if len == self.arena_cap() {
            let new_start = arena.len() as u32;
            let old = self.start as usize;
            arena.extend_from_within(old..old + len as usize);
            self.start = new_start;
            arena.resize(new_start as usize + 2 * len as usize, (0, 0));
        }
        arena[(self.start + len) as usize] = e;
        self.len += 1;
    }
}
