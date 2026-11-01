use super::{BestNode, EndSlot, LeftCandidate, MAX_NODE_COST, MAX_PATH_LINKS};
use crate::error::Error;
use crate::ids::NodeId;

#[inline(always)]
pub(super) fn prefetch<T>(address: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(address.cast());
    };
    #[cfg(not(target_arch = "x86_64"))]
    let _ = address;
}

#[inline(always)]
pub(super) fn best_left<const SELECT: bool>(
    lefts: &[LeftCandidate],
    mut connection_cost: impl FnMut(u16) -> i16,
) -> (i32, NodeId) {
    let mut best_cost = i32::MAX;
    let mut best = lefts[0].id;
    for left in lefts {
        let cost = left.cost + i32::from(connection_cost(left.right_id));
        if SELECT {
            (best_cost, best) = std::hint::select_unpredictable(
                cost <= best_cost,
                (cost, left.id),
                (best_cost, best),
            );
        } else if cost <= best_cost {
            best_cost = cost;
            best = left.id;
        }
    }
    (best_cost, best)
}

#[cold]
pub(super) fn overflow_result(overflow: bool) -> Result<(), Error> {
    if overflow {
        Err(Error::TextTooLong)
    } else {
        Ok(())
    }
}

#[inline(always)]
pub(super) fn cost_in_range(cost: i32) -> bool {
    cost.unsigned_abs() <= MAX_NODE_COST.unsigned_abs()
}

pub(super) fn links_fit(existing: usize, added: usize) -> bool {
    existing
        .checked_add(added)
        .is_some_and(|total| total <= MAX_PATH_LINKS)
}

#[inline(always)]
pub(super) fn store_best(
    best_nodes: &mut [BestNode],
    ends: &mut [EndSlot],
    position: usize,
    index: usize,
    cost_without_word: i32,
    best: NodeId,
    overflow: &mut bool,
) {
    let node = &mut best_nodes[index];
    let cost = cost_without_word + i32::from(node.candidate.word_cost);
    *overflow |= !cost_in_range(cost);
    node.prev = Some(best);
    node.cost = cost;
    ends[node.candidate.end as usize - position - 1].push(LeftCandidate {
        right_id: node.candidate.right_id,
        cost,
        id: NodeId::from_index(index),
    });
}
