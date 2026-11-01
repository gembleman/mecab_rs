use super::big::Big;
use super::table::Table;
use super::{BIG_AT, TOTAL_LIMIT};

#[inline]
pub(super) fn reinforce(
    t: &mut Table,
    arena: &mut [(u32, u32)],
    bigs: &mut [Big],
    pos: usize,
    inc: u32,
) {
    if t.is_big() {
        bigs[t.big_index()].add(pos, inc);
    } else {
        move_up(t.entries_mut(arena), pos, inc);
    }
}

#[inline]
fn move_up(e: &mut [(u32, u32)], i: usize, inc: u32) {
    let new = e[i].1 + inc;
    let j = e[..i].partition_point(|x| x.1 >= new);
    e[j..=i].rotate_right(1);
    e[j].1 = new;
}

#[inline]
pub(super) fn learn_symbol(
    t: &mut Table,
    arena: &mut Vec<(u32, u32)>,
    bigs: &mut Vec<Big>,
    sym: u32,
) {
    if t.is_big() {
        bigs[t.big_index()].push(sym, 1);
        t.count_one_more();
    } else if t.len() >= BIG_AT {
        promote(t, arena, bigs, sym);
        t.count_one_more();
    } else {
        t.push(arena, (sym, 1));
    }
}

fn promote(t: &mut Table, arena: &[(u32, u32)], bigs: &mut Vec<Big>, sym: u32) {
    let mut big = Big::from_sorted(t.entries(arena));
    big.push(sym, 1);
    t.set_big_index(bigs.len());
    bigs.push(big);
}

#[inline]
pub(super) fn halve_over_limit(t: &mut Table, arena: &mut [(u32, u32)], bigs: &mut [Big]) {
    if t.total() + t.len() <= TOTAL_LIMIT {
        return;
    }
    let total = if t.is_big() {
        bigs[t.big_index()].halve()
    } else {
        halve_counts(t.entries_mut(arena))
    };
    t.set_total(total);
}

fn halve_counts(e: &mut [(u32, u32)]) -> u32 {
    let mut total = 0;
    for entry in e {
        entry.1 = entry.1.div_ceil(2);
        total += entry.1;
    }
    total
}
