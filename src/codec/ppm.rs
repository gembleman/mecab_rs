use super::coder::Coder;
use crate::wire::FNV_PRIME;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(FNV_PRIME);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = x;
    }
}

pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;

#[inline]
pub fn mix(h: u64, x: u64) -> u64 {
    let mut z = h
        .wrapping_mul(0xFF51_AFD7_ED55_8CCD)
        .wrapping_add(x.wrapping_mul(0xC4CE_B9FE_1A85_EC53))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0x243F_6A88_85A3_08D3 ^ b.len() as u64;
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

#[derive(Default)]
pub struct Interner {
    map: FastMap<u64, u32>,
    buf: Vec<u8>,
    ends: Vec<usize>,
}

impl Interner {
    pub fn get(&self, id: u32) -> &[u8] {
        let i = id as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.buf[start..self.ends[i]]
    }

    pub fn find(&self, key: &[u8]) -> Option<u32> {
        let mut h = hash_bytes(key);
        loop {
            match self.map.get(&h) {
                None => return None,
                Some(&id) if self.get(id) == key => return Some(id),
                Some(_) => h = h.wrapping_add(1),
            }
        }
    }

    pub fn add(&mut self, key: &[u8]) -> u32 {
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
}

const TOTAL_LIMIT: u32 = 60000;
const MAX_SYMBOLS: u32 = 24000;
pub const NOVEL: u32 = u32::MAX;
const MAX_LEVELS: usize = 8;

const BIG_AT: u32 = 24;
const BIG: u32 = u32::MAX;
const HIT_INC: u32 = 2;
const EXCLUSION_CAP: usize = 48;
const EXCLUSION_SCAN_MAX: usize = 24;
const BIG_INITIAL_CAP: usize = 64;
const INITIAL_SLOTS: usize = 32;

#[derive(Clone, Copy, Default)]
struct Table {
    total: u32,
    len: u32,
    cap: u32,
    start: u32,
    inl: [(u32, u32); 2],
}

impl Table {
    #[inline]
    fn entries<'a>(&'a self, arena: &'a [(u32, u32)]) -> &'a [(u32, u32)] {
        if self.cap == 0 {
            &self.inl[..self.len as usize]
        } else {
            &arena[self.start as usize..(self.start + self.len) as usize]
        }
    }

    #[inline]
    fn entries_mut<'a>(&'a mut self, arena: &'a mut [(u32, u32)]) -> &'a mut [(u32, u32)] {
        if self.cap == 0 {
            &mut self.inl[..self.len as usize]
        } else {
            &mut arena[self.start as usize..(self.start + self.len) as usize]
        }
    }

    fn push(&mut self, arena: &mut Vec<(u32, u32)>, e: (u32, u32)) {
        if self.cap == 0 {
            if self.len < 2 {
                self.inl[self.len as usize] = e;
                self.len += 1;
                return;
            }
            self.start = arena.len() as u32;
            self.cap = 4;
            arena.extend_from_slice(&self.inl);
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

struct Big {
    syms: Vec<u32>,
    cnt: Vec<u32>,
    tree: Vec<u32>,
    pos_of: FastMap<u32, u32>,
    pos_ready: bool,
}

impl Big {
    fn from_sorted(entries: &[(u32, u32)]) -> Big {
        let mut b = Big {
            syms: entries.iter().map(|e| e.0).collect(),
            cnt: entries.iter().map(|e| e.1).collect(),
            tree: Vec::new(),
            pos_of: FastMap::default(),
            pos_ready: false,
        };
        b.rebuild(BIG_INITIAL_CAP);
        b
    }

    fn rebuild(&mut self, cap: usize) {
        self.tree.clear();
        self.tree.resize(cap + 1, 0);
        for (i, &c) in self.cnt.iter().enumerate() {
            self.tree[i + 1] = c;
        }
        for i in 1..=cap {
            let j = i + i.isolate_lowest_one();
            if j <= cap {
                let v = self.tree[i];
                self.tree[j] += v;
            }
        }
    }

    fn cap(&self) -> usize {
        self.tree.len() - 1
    }

    fn add(&mut self, pos: usize, delta: u32) {
        self.cnt[pos] += delta;
        let cap = self.cap();
        let mut i = pos + 1;
        while i <= cap {
            self.tree[i] += delta;
            i += i.isolate_lowest_one();
        }
    }

    fn push(&mut self, sym: u32, count: u32) {
        let pos = self.syms.len();
        if self.pos_ready {
            self.pos_of.insert(sym, pos as u32);
        }
        self.syms.push(sym);
        self.cnt.push(0);
        if pos + 1 > self.cap() {
            let cap = self.cap() * 2;
            self.rebuild(cap);
        }
        self.add(pos, count);
    }

    fn position(&mut self, sym: u32) -> Option<usize> {
        if !self.pos_ready {
            for (i, &x) in self.syms.iter().enumerate() {
                self.pos_of.insert(x, i as u32);
            }
            self.pos_ready = true;
        }
        self.pos_of.get(&sym).map(|&p| p as usize)
    }

    fn prefix(&self, pos: usize) -> u32 {
        let mut i = pos;
        let mut s = 0;
        while i > 0 {
            s += self.tree[i];
            i -= i.isolate_lowest_one();
        }
        s
    }

    fn find(&self, mut target: u32) -> (usize, u32) {
        let cap = self.cap();
        let start = target;
        let mut pos = 0usize;
        let mut step = cap;
        while step > 0 {
            let next = pos + step;
            if next <= cap && self.tree[next] <= target {
                pos = next;
                target -= self.tree[next];
            }
            step >>= 1;
        }
        (pos, start - target)
    }

    fn halve(&mut self) -> u32 {
        let mut total = 0;
        for c in &mut self.cnt {
            *c = c.div_ceil(2);
            total += *c;
        }
        let cap = self.cap();
        self.rebuild(cap);
        total
    }
}

#[derive(Clone, Copy, Default)]
struct Slot {
    key: u64,
    t: Table,
}

pub struct Ppm {
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

impl Ppm {
    #[inline]
    fn slot_for(&mut self, h: u64) -> u32 {
        let h = h | 1;
        let mut i = h as usize & self.mask;
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

    pub fn distinct(&self, h: u64) -> u32 {
        let h = h | 1;
        let mut i = h as usize & self.mask;
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
    pub fn prefetch(&self, h: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            let i = (h | 1) as usize & self.mask;
            _mm_prefetch(self.slots.as_ptr().add(i) as *const i8, _MM_HINT_T0);
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
                let mut i = s.key as usize & self.mask;
                while self.slots[i].key != 0 {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = s;
            }
        }
    }

    #[inline]
    pub fn code(&mut self, c: &mut Coder, ctxs: &[u64], sym: u32) -> Option<u32> {
        if (self.used + MAX_LEVELS) * 2 > self.slots.len() {
            self.grow();
        }
        self.nvisited = 0;
        self.hit = false;
        let mut excl = [0u32; EXCLUSION_CAP];
        let mut nex = 0usize;
        for &h in ctxs {
            let si = self.slot_for(h);
            self.visited[self.nvisited] = si;
            self.nvisited += 1;
            let t = &self.slots[si as usize].t;
            if t.total == 0 {
                continue;
            }
            let d = t.len;
            let total = t.total + d;
            if t.cap == BIG {
                let big = &mut self.bigs[t.start as usize];
                if c.is_decoder() {
                    let tg = c.target(total);
                    if tg < t.total {
                        let (pos, cum) = big.find(tg);
                        c.consume(cum, big.cnt[pos]);
                        self.hit_pos = pos;
                        self.hit = true;
                        return Some(big.syms[pos]);
                    }
                    c.consume(t.total, d);
                } else if let Some(pos) = big.position(sym) {
                    c.put(big.prefix(pos), big.cnt[pos], total);
                    self.hit_pos = pos;
                    self.hit = true;
                    return Some(sym);
                } else {
                    c.put(t.total, d, total);
                }
                continue;
            }
            let entries = t.entries(&self.arena);
            let (mut tot, mut dist) = (t.total, d);
            if nex > 0 {
                for e in entries {
                    if excl[..nex].contains(&e.0) {
                        tot -= e.1;
                        dist -= 1;
                    }
                }
            }
            if tot > 0 {
                let total = tot + dist;
                if c.is_decoder() {
                    let tg = c.target(total);
                    if tg < tot {
                        let mut cum = 0;
                        for (i, &(s, n)) in entries.iter().enumerate() {
                            if nex > 0 && excl[..nex].contains(&s) {
                                continue;
                            }
                            if tg < cum + n {
                                c.consume(cum, n);
                                self.hit_pos = i;
                                self.hit = true;
                                return Some(s);
                            }
                            cum += n;
                        }
                        unreachable!("frequency table out of sync");
                    }
                    c.consume(tot, dist);
                } else {
                    let mut cum = 0;
                    for (i, &(s, n)) in entries.iter().enumerate() {
                        if nex > 0 && excl[..nex].contains(&s) {
                            continue;
                        }
                        if s == sym {
                            c.put(cum, n, total);
                            self.hit_pos = i;
                            self.hit = true;
                            return Some(sym);
                        }
                        cum += n;
                    }
                    c.put(tot, dist, total);
                }
            }
            if entries.len() <= EXCLUSION_SCAN_MAX {
                for e in entries {
                    if nex < excl.len() && !excl[..nex].contains(&e.0) {
                        excl[nex] = e.0;
                        nex += 1;
                    }
                }
            }
        }
        None
    }

    #[inline]
    pub fn update(&mut self, sym: u32) {
        let n = self.nvisited;
        for k in 0..n {
            let t = &mut self.slots[self.visited[k] as usize].t;
            let mut inc = 1;
            if self.hit && k == n - 1 {
                inc = HIT_INC;
                let i = self.hit_pos;
                if t.cap == BIG {
                    self.bigs[t.start as usize].add(i, inc);
                } else {
                    let e = t.entries_mut(&mut self.arena);
                    let new = e[i].1 + inc;
                    let j = e[..i].partition_point(|x| x.1 >= new);
                    e[j..=i].rotate_right(1);
                    e[j].1 = new;
                }
            } else if t.len < MAX_SYMBOLS {
                if t.cap == BIG {
                    self.bigs[t.start as usize].push(sym, 1);
                } else if t.len >= BIG_AT {
                    let big = Big::from_sorted(t.entries(&self.arena));
                    t.start = self.bigs.len() as u32;
                    t.cap = BIG;
                    self.bigs.push(big);
                    self.bigs[t.start as usize].push(sym, 1);
                } else {
                    t.push(&mut self.arena, (sym, 1));
                }
                t.len += (t.cap == BIG) as u32;
            } else {
                continue;
            }
            t.total += inc;
            if t.total + t.len > TOTAL_LIMIT {
                if t.cap == BIG {
                    t.total = self.bigs[t.start as usize].halve();
                } else {
                    let mut total = 0;
                    for e in t.entries_mut(&mut self.arena) {
                        e.1 = e.1.div_ceil(2);
                        total += e.1;
                    }
                    t.total = total;
                }
            }
        }
    }
}
