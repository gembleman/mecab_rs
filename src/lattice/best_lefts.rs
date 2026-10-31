use super::{CONNECT_GROUP_SIZE, LeftCandidate};
use crate::ids::NodeId;

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
pub(super) fn best_lefts(
    lefts: &[LeftCandidate],
    columns: [&[i16]; CONNECT_GROUP_SIZE],
) -> [(i32, NodeId); CONNECT_GROUP_SIZE] {
    let mut best = [(i32::MAX, lefts[0].id); CONNECT_GROUP_SIZE];
    for left in lefts {
        let right_id = usize::from(left.right_id);
        for (best, cells) in best.iter_mut().zip(columns) {
            debug_assert!(right_id < cells.len());
            let cost = left.cost + i32::from(unsafe { *cells.get_unchecked(right_id) });
            *best = std::hint::select_unpredictable(cost <= best.0, (cost, left.id), *best);
        }
    }
    best
}

#[cfg(target_arch = "x86_64")]
const SIGN_EXTEND_SHIFT: i32 = 16;

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn pick_where_set(
    mask: std::arch::x86_64::__m128i,
    chosen: std::arch::x86_64::__m128i,
    otherwise: std::arch::x86_64::__m128i,
) -> std::arch::x86_64::__m128i {
    use std::arch::x86_64::{_mm_and_si128, _mm_andnot_si128, _mm_or_si128};
    unsafe {
        _mm_or_si128(
            _mm_and_si128(mask, chosen),
            _mm_andnot_si128(mask, otherwise),
        )
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn gather_cells(
    columns: &[&[i16]; CONNECT_GROUP_SIZE],
    right_id: usize,
) -> std::arch::x86_64::__m128i {
    for cells in columns {
        debug_assert!(right_id < cells.len());
    }
    unsafe {
        std::arch::x86_64::_mm_setr_epi16(
            *columns[0].get_unchecked(right_id),
            *columns[1].get_unchecked(right_id),
            *columns[2].get_unchecked(right_id),
            *columns[3].get_unchecked(right_id),
            0,
            0,
            0,
            0,
        )
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(super) fn best_lefts(
    lefts: &[LeftCandidate],
    columns: [&[i16]; CONNECT_GROUP_SIZE],
) -> [(i32, NodeId); CONNECT_GROUP_SIZE] {
    use std::arch::x86_64::{
        _mm_add_epi32, _mm_cmpgt_epi32, _mm_set1_epi32, _mm_setzero_si128, _mm_srai_epi32,
        _mm_storeu_si128, _mm_unpacklo_epi16,
    };

    debug_assert!(!lefts.is_empty());
    debug_assert!(i32::try_from(lefts.len()).is_ok());
    let mut costs = [0i32; CONNECT_GROUP_SIZE];
    let mut positions = [0i32; CONNECT_GROUP_SIZE];
    unsafe {
        let mut best_cost = _mm_set1_epi32(i32::MAX);
        let mut best_position = _mm_setzero_si128();
        let mut position = _mm_setzero_si128();
        let one = _mm_set1_epi32(1);
        for left in lefts {
            let cells = gather_cells(&columns, usize::from(left.right_id));
            let widened = _mm_srai_epi32::<SIGN_EXTEND_SHIFT>(_mm_unpacklo_epi16(cells, cells));
            let cost = _mm_add_epi32(_mm_set1_epi32(left.cost), widened);
            let worse = _mm_cmpgt_epi32(cost, best_cost);
            best_cost = pick_where_set(worse, best_cost, cost);
            best_position = pick_where_set(worse, best_position, position);
            position = _mm_add_epi32(position, one);
        }
        _mm_storeu_si128(costs.as_mut_ptr().cast(), best_cost);
        _mm_storeu_si128(positions.as_mut_ptr().cast(), best_position);
    }
    std::array::from_fn(|lane| (costs[lane], lefts[positions[lane] as usize].id))
}
