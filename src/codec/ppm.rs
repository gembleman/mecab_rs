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
pub(crate) const MAX_LEVELS: usize = 8;

const BIG_AT: u32 = 24;
const HIT_INC: u32 = 2;
const EXCLUSION_CAP: usize = 48;
const EXCLUSION_SCAN_MAX: usize = 24;
const INITIAL_SLOTS: usize = 32;
const LOAD_NUM: usize = 3;
const LOAD_DEN: usize = 4;
const MAX_HINT_CONTEXTS: u32 = 1 << 26;
const MAX_HINT_ARENA: u32 = 1 << 27;

#[derive(Clone, Copy, Default)]
struct Slot {
    key: u64,
    t: Table,
}

const _: () = assert!(size_of::<Slot>() == 32);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct PpmHint {
    pub(crate) contexts: u32,
    pub(crate) arena: u32,
}

pub(crate) struct Ppm {
    slots: Vec<Slot>,
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
        Ppm::with_hint(PpmHint::default())
    }
}

#[inline]
fn home(key: u64, slot_count: usize) -> usize {
    ((u128::from(key) * slot_count as u128) >> u64::BITS) as usize
}

#[inline]
fn next_probe(i: usize, slot_count: usize) -> usize {
    let next = i + 1;
    if next == slot_count { 0 } else { next }
}

impl Ppm {
    pub(crate) fn with_hint(hint: PpmHint) -> Ppm {
        let contexts = hint.contexts.min(MAX_HINT_CONTEXTS) as usize;
        let arena = hint.arena.min(MAX_HINT_ARENA) as usize;
        let needed = (contexts + MAX_LEVELS) * LOAD_DEN;
        let slot_count = needed.div_ceil(LOAD_NUM).max(INITIAL_SLOTS);
        Ppm {
            slots: vec![Slot::default(); slot_count],
            used: 0,
            arena: Vec::with_capacity(arena),
            bigs: Vec::new(),
            visited: [0; MAX_LEVELS],
            nvisited: 0,
            hit_pos: 0,
            hit: false,
        }
    }

    pub(crate) fn hint(&self) -> PpmHint {
        PpmHint {
            contexts: u32::try_from(self.used).unwrap_or(u32::MAX),
            arena: u32::try_from(self.arena.len()).unwrap_or(u32::MAX),
        }
    }

    #[inline]
    fn slot_for(&mut self, h: u64) -> u32 {
        let h = h | 1;
        let n = self.slots.len();
        let mut i = home(h, n);
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
            i = next_probe(i, n);
        }
    }

    pub(crate) fn distinct(&self, h: u64) -> u32 {
        let h = h | 1;
        let n = self.slots.len();
        let mut i = home(h, n);
        loop {
            let k = self.slots[i].key;
            if k == h {
                return self.slots[i].t.len();
            }
            if k == 0 {
                return 0;
            }
            i = next_probe(i, n);
        }
    }

    #[inline]
    pub(crate) fn prefetch(&self, h: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            let i = home(h | 1, self.slots.len());
            _mm_prefetch(self.slots.as_ptr().add(i).cast::<i8>(), _MM_HINT_T0);
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = h;
    }

    #[cold]
    fn grow(&mut self) {
        let n = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![Slot::default(); n]);
        for s in old {
            if s.key != 0 {
                let mut i = home(s.key, n);
                while self.slots[i].key != 0 {
                    i = next_probe(i, n);
                }
                self.slots[i] = s;
            }
        }
    }

    #[inline]
    fn ensure_room(&mut self) {
        if (self.used + MAX_LEVELS) * LOAD_DEN > self.slots.len() * LOAD_NUM {
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
        if t.total() == 0 {
            return None;
        }
        let mass = Mass {
            total: t.total(),
            distinct: t.len(),
        };
        if t.is_big() {
            return code_big(&mut self.bigs[t.big_index()], mass, coder, sym);
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
            if t.len() >= MAX_SYMBOLS {
                return;
            }
            learn::learn_symbol(t, &mut self.arena, &mut self.bigs, sym);
            1
        };
        t.add_total(inc);
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
