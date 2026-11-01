use super::EXCLUSION_CAP;
use super::big::Big;
use crate::codec::coder::Coder;

#[derive(Clone, Copy)]
pub(super) struct Mass {
    pub(super) total: u32,
    pub(super) distinct: u32,
}

impl Mass {
    #[inline]
    fn span(self) -> u32 {
        self.total + self.distinct
    }
}

pub(super) struct Exclusions {
    syms: [u32; EXCLUSION_CAP],
    len: usize,
}

impl Exclusions {
    #[inline]
    pub(super) fn new() -> Self {
        Exclusions {
            syms: [0; EXCLUSION_CAP],
            len: 0,
        }
    }

    #[inline]
    fn hides(&self, s: u32) -> bool {
        self.syms[..self.len].contains(&s)
    }

    #[inline]
    pub(super) fn add_all(&mut self, entries: &[(u32, u32)]) {
        for &(s, _) in entries {
            if self.len < EXCLUSION_CAP && !self.hides(s) {
                self.syms[self.len] = s;
                self.len += 1;
            }
        }
    }
}

#[inline]
fn open_mass(mass: Mass, entries: &[(u32, u32)], excluded: &Exclusions) -> Mass {
    let mut open = mass;
    if excluded.len > 0 {
        for &(s, n) in entries {
            if excluded.hides(s) {
                open.total -= n;
                open.distinct -= 1;
            }
        }
    }
    open
}

#[inline]
pub(super) fn code_small(
    mass: Mass,
    entries: &[(u32, u32)],
    coder: &mut Coder,
    sym: u32,
    excluded: &Exclusions,
) -> Option<(usize, u32)> {
    let open = open_mass(mass, entries, excluded);
    if open.total == 0 {
        return None;
    }
    if coder.is_decoder() {
        decode_small(open, entries, coder, excluded)
    } else {
        encode_small(open, entries, coder, sym, excluded)
    }
}

#[inline]
fn decode_small(
    open: Mass,
    entries: &[(u32, u32)],
    coder: &mut Coder,
    excluded: &Exclusions,
) -> Option<(usize, u32)> {
    let target = coder.target(open.span());
    if target >= open.total {
        coder.consume(open.total, open.distinct);
        return None;
    }
    let mut cumulative = 0;
    for (i, &(s, n)) in entries.iter().enumerate() {
        if excluded.hides(s) {
            continue;
        }
        if target < cumulative + n {
            coder.consume(cumulative, n);
            return Some((i, s));
        }
        cumulative += n;
    }
    unreachable!("frequency table out of sync");
}

#[inline]
fn encode_small(
    open: Mass,
    entries: &[(u32, u32)],
    coder: &mut Coder,
    sym: u32,
    excluded: &Exclusions,
) -> Option<(usize, u32)> {
    let total = open.span();
    let mut cumulative = 0;
    for (i, &(s, n)) in entries.iter().enumerate() {
        if excluded.hides(s) {
            continue;
        }
        if s == sym {
            coder.put(cumulative, n, total);
            return Some((i, sym));
        }
        cumulative += n;
    }
    coder.put(open.total, open.distinct, total);
    None
}

#[inline]
pub(super) fn code_big(
    big: &mut Big,
    mass: Mass,
    coder: &mut Coder,
    sym: u32,
) -> Option<(usize, u32)> {
    if coder.is_decoder() {
        decode_big(big, mass, coder)
    } else {
        encode_big(big, mass, coder, sym)
    }
}

#[inline]
fn decode_big(big: &Big, mass: Mass, coder: &mut Coder) -> Option<(usize, u32)> {
    let target = coder.target(mass.span());
    if target < mass.total {
        let (pos, cumulative) = big.find(target);
        coder.consume(cumulative, big.counts[pos]);
        return Some((pos, big.syms[pos]));
    }
    coder.consume(mass.total, mass.distinct);
    None
}

#[inline]
fn encode_big(big: &mut Big, mass: Mass, coder: &mut Coder, sym: u32) -> Option<(usize, u32)> {
    let total = mass.span();
    if let Some(pos) = big.position(sym) {
        coder.put(big.prefix(pos), big.counts[pos], total);
        return Some((pos, sym));
    }
    coder.put(mass.total, mass.distinct, total);
    None
}
