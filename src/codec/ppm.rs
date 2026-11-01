use self::big::Big;
use self::level::{Exclusions, Mass, code_big, code_small};
use self::table::Table;
use super::coder::Coder;

mod big;
mod hash;
mod intern;
mod learn;
mod level;
mod table;

pub(crate) use hash::{hash_bytes, mix};
pub(crate) use intern::Interner;

const TOTAL_LIMIT: u32 = 60000;
const MAX_SYMBOLS: u32 = 24000;
pub(crate) const NOVEL: u32 = u32::MAX;
const MAX_LEVELS: usize = 8;

const BIG_AT: u32 = 24;
const BIG: u32 = u32::MAX;
const HIT_INC: u32 = 2;
const EXCLUSION_CAP: usize = 48;
const EXCLUSION_SCAN_MAX: usize = 24;
const INITIAL_SLOTS: usize = 32;

#[derive(Clone, Copy, Default)]
struct Slot {
    key: u64,
    t: Table,
}

pub(crate) struct Ppm {
    slots: Vec<Slot>,
    mask: usize,
    used: usize,
    arena: Vec<(u32, u32)>,
    bigs: Vec<Big>,
    visited: [u32; MAX_LEVELS],
    nvisited: usize,
    hit_pos: usize,
    hit: bool,
}

impl Default for Ppm {
    fn default() -> Self {
        Ppm {
            slots: vec![Slot::default(); INITIAL_SLOTS],
            mask: INITIAL_SLOTS - 1,
            used: 0,
            arena: Vec::new(),
            bigs: Vec::new(),
            visited: [0; MAX_LEVELS],
            nvisited: 0,
            hit_pos: 0,
            hit: false,
        }
    }
}

#[inline]
fn home(key: u64, mask: usize) -> usize {
    (key >> 1) as usize & mask
}

impl Ppm {
    #[inline]
    fn slot_for(&mut self, h: u64) -> u32 {
        let h = h | 1;
        let mut i = home(h, self.mask);
        loop {
            let k = self.slots[i].key;
            if k == h {
                return i as u32;
            }
            if k == 0 {
                self.slots[i].key = h;
                self.used += 1;
                return i as u32;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub(crate) fn distinct(&self, h: u64) -> u32 {
        let h = h | 1;
        let mut i = home(h, self.mask);
        loop {
            let k = self.slots[i].key;
            if k == h {
                return self.slots[i].t.len;
            }
            if k == 0 {
                return 0;
            }
            i = (i + 1) & self.mask;
        }
    }

    #[inline]
    pub(crate) fn prefetch(&self, h: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            let i = home(h | 1, self.mask);
            _mm_prefetch(self.slots.as_ptr().add(i).cast::<i8>(), _MM_HINT_T0);
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = h;
    }

    #[cold]
    fn grow(&mut self) {
        let n = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![Slot::default(); n]);
        self.mask = n - 1;
        for s in old {
            if s.key != 0 {
                let mut i = home(s.key, self.mask);
                while self.slots[i].key != 0 {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = s;
            }
        }
    }

    #[inline]
    fn ensure_room(&mut self) {
        if (self.used + MAX_LEVELS) * 2 > self.slots.len() {
            self.grow();
        }
    }

    #[inline]
    fn visit(&mut self, h: u64) -> usize {
        let slot = self.slot_for(h) as usize;
        self.visited[self.nvisited] = slot as u32;
        self.nvisited += 1;
        slot
    }

    #[inline]
    fn code_level(
        &mut self,
        coder: &mut Coder,
        slot: usize,
        sym: u32,
        excluded: &mut Exclusions,
    ) -> Option<(usize, u32)> {
        let t = &self.slots[slot].t;
        if t.total == 0 {
            return None;
        }
        let mass = Mass {
            total: t.total,
            distinct: t.len,
        };
        if t.cap == BIG {
            return code_big(&mut self.bigs[t.start as usize], mass, coder, sym);
        }
        let entries = t.entries(&self.arena);
        let found = code_small(mass, entries, coder, sym, excluded);
        if entries.len() <= EXCLUSION_SCAN_MAX {
            excluded.add_all(entries);
        }
        found
    }

    #[inline]
    pub(crate) fn code(&mut self, coder: &mut Coder, ctxs: &[u64], sym: u32) -> Option<u32> {
        debug_assert!(ctxs.len() <= MAX_LEVELS);
        self.ensure_room();
        self.nvisited = 0;
        self.hit = false;
        let mut excluded = Exclusions::new();
        for &h in ctxs {
            let slot = self.visit(h);
            if let Some((pos, s)) = self.code_level(coder, slot, sym, &mut excluded) {
                self.hit_pos = pos;
                self.hit = true;
                return Some(s);
            }
        }
        None
    }

    #[inline]
    fn update_level(&mut self, slot: usize, sym: u32, reinforce_at: Option<usize>) {
        let t = &mut self.slots[slot].t;
        let inc = if let Some(pos) = reinforce_at {
            learn::reinforce(t, &mut self.arena, &mut self.bigs, pos, HIT_INC);
            HIT_INC
        } else {
            if t.len >= MAX_SYMBOLS {
                return;
            }
            learn::learn_symbol(t, &mut self.arena, &mut self.bigs, sym);
            1
        };
        t.total += inc;
        learn::halve_over_limit(t, &mut self.arena, &mut self.bigs);
    }

    #[inline]
    pub(crate) fn update(&mut self, sym: u32) {
        let n = self.nvisited;
        for k in 0..n {
            let reinforce_at = if self.hit && k + 1 == n {
                Some(self.hit_pos)
            } else {
                None
            };
            let slot = self.visited[k] as usize;
            self.update_level(slot, sym, reinforce_at);
        }
    }
}
