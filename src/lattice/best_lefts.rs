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
const PAIR_SHIFT: u32 = 16;

#[cfg(target_arch = "x86_64")]
const HALF_SHIFT: u32 = 32;

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn pack_pair(first: &[i16], second: &[i16], right_id: usize) -> u64 {
    let (first, second) = unsafe {
        (
            *first.get_unchecked(right_id),
            *second.get_unchecked(right_id),
        )
    };
    u64::from(first as u16) | (u64::from(second as u16) << PAIR_SHIFT)
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
        let low = pack_pair(columns[0], columns[1], right_id);
        let high = pack_pair(columns[2], columns[3], right_id);
        std::arch::x86_64::_mm_cvtsi64_si128((low | (high << HALF_SHIFT)) as i64)
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(super) fn best_lefts(
    lefts: &[LeftCandidate],
    columns: [&[i16]; CONNECT_GROUP_SIZE],
) -> [(i32, NodeId); CONNECT_GROUP_SIZE] {
    use std::arch::x86_64::{
        _mm_add_epi32, _mm_and_si128, _mm_cmpgt_epi32, _mm_set1_epi32, _mm_setzero_si128,
        _mm_srai_epi32, _mm_storeu_si128, _mm_sub_epi32, _mm_unpacklo_epi16,
    };

    debug_assert!(!lefts.is_empty());
    debug_assert!(i32::try_from(lefts.len()).is_ok());
    let mut costs = [0i32; CONNECT_GROUP_SIZE];
    let mut behind = [0i32; CONNECT_GROUP_SIZE];
    unsafe {
        let mut best_cost = _mm_set1_epi32(i32::MAX);
        let mut lefts_since_best = _mm_setzero_si128();
        for left in lefts {
            let cells = gather_cells(&columns, usize::from(left.right_id));
            let widened = _mm_srai_epi32::<SIGN_EXTEND_SHIFT>(_mm_unpacklo_epi16(cells, cells));
            let cost = _mm_add_epi32(_mm_set1_epi32(left.cost), widened);
            let worse = _mm_cmpgt_epi32(cost, best_cost);
            best_cost = pick_where_set(worse, best_cost, cost);
            lefts_since_best = _mm_and_si128(_mm_sub_epi32(lefts_since_best, worse), worse);
        }
        _mm_storeu_si128(costs.as_mut_ptr().cast(), best_cost);
        _mm_storeu_si128(behind.as_mut_ptr().cast(), lefts_since_best);
    }
    let last = lefts.len() - 1;
    std::array::from_fn(|lane| {
        let position = last - behind[lane] as usize;
        debug_assert!(position < lefts.len());
        (costs[lane], unsafe { lefts.get_unchecked(position) }.id)
    })
}
