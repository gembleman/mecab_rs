use super::hash::FastMap;

const BIG_INITIAL_CAP: usize = 64;

pub(super) struct Big {
    pub(super) syms: Vec<u32>,
    pub(super) counts: Vec<u32>,
    tree: Vec<u32>,
    pos_of: FastMap<u32, u32>,
    pos_ready: bool,
}

impl Big {
    pub(super) fn from_sorted(entries: &[(u32, u32)]) -> Big {
        let mut b = Big {
            syms: entries.iter().map(|e| e.0).collect(),
            counts: entries.iter().map(|e| e.1).collect(),
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
        for (i, &count) in self.counts.iter().enumerate() {
            self.tree[i + 1] = count;
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

    pub(super) fn add(&mut self, pos: usize, delta: u32) {
        self.counts[pos] += delta;
        let cap = self.cap();
        let mut i = pos + 1;
        while i <= cap {
            self.tree[i] += delta;
            i += i.isolate_lowest_one();
        }
    }

    pub(super) fn push(&mut self, sym: u32, count: u32) {
        let pos = self.syms.len();
        if self.pos_ready {
            self.pos_of.insert(sym, pos as u32);
        }
        self.syms.push(sym);
        self.counts.push(0);
        if pos + 1 > self.cap() {
            let cap = self.cap() * 2;
            self.rebuild(cap);
        }
        self.add(pos, count);
    }

    pub(super) fn position(&mut self, sym: u32) -> Option<usize> {
        if !self.pos_ready {
            for (i, &x) in self.syms.iter().enumerate() {
                self.pos_of.insert(x, i as u32);
            }
            self.pos_ready = true;
        }
        self.pos_of.get(&sym).map(|&p| p as usize)
    }

    pub(super) fn prefix(&self, pos: usize) -> u32 {
        let mut i = pos;
        let mut s = 0;
        while i > 0 {
            s += self.tree[i];
            i -= i.isolate_lowest_one();
        }
        s
    }

    pub(super) fn find(&self, mut target: u32) -> (usize, u32) {
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

    pub(super) fn halve(&mut self) -> u32 {
        let mut total = 0;
        for count in &mut self.counts {
            *count = count.div_ceil(2);
            total += *count;
        }
        let cap = self.cap();
        self.rebuild(cap);
        total
    }
}
