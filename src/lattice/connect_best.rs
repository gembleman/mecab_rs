use super::best_lefts::best_lefts;
use super::cost::{best_left, overflow_result, prefetch, store_best};
use super::{BestNode, BestSite, CONNECT_GROUP_SIZE, EndSlot, Lattice, LeftCandidate};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::matrix::{DenseColumns, DenseMatrix, HotView, Matrix};

#[cfg(feature = "pair-stats")]
fn record_pairs(nodes: &[BestNode], lefts: &[LeftCandidate]) {
    for node in nodes {
        for left in lefts {
            crate::pair_stats::record(node.candidate.left_id, left.right_id);
        }
    }
}

#[inline(always)]
fn prefetch_columns(
    nodes: &[BestNode],
    lefts: &[LeftCandidate],
    hot: &HotView<'_>,
    reaches: impl Fn(&[i16]) -> bool,
) {
    for node in nodes {
        let cells = hot.right_column(node.candidate.left_id).cells();
        if reaches(cells) {
            for left in lefts {
                prefetch(cells.as_ptr().wrapping_add(usize::from(left.right_id)));
            }
        }
    }
}
const DENSE_GROUPED: bool = true;

impl Lattice {
    pub(super) fn connect_best(
        &mut self,
        dictionary: &Dictionary,
        position: usize,
        first: usize,
    ) -> Result<(), Error> {
        let Self {
            best_nodes,
            ends,
            cost_overflow,
            ..
        } = self;
        let (head, later_ends) = ends.split_at_mut(position + 1);
        let site = BestSite {
            lefts: &head[position].lefts,
            widest: head[position].widest,
            position,
            first,
        };
        match &dictionary.matrix {
            Matrix::PackedDense(dense) => {
                Self::connect_best_dense(best_nodes, later_ends, &site, dense, cost_overflow);
            }
            Matrix::PackedHot(hot) => {
                Self::connect_best_hot(best_nodes, later_ends, &site, &hot.view(), cost_overflow);
            }
        }
        overflow_result(*cost_overflow)
    }

    #[inline(always)]
    fn connect_best_dense(
        best_nodes: &mut [BestNode],
        later_ends: &mut [EndSlot],
        site: &BestSite<'_>,
        dense: &DenseMatrix,
        overflow: &mut bool,
    ) {
        let columns = dense.columns();
        let mut next = best_nodes.len();
        while next > site.first {
            if DENSE_GROUPED && next - site.first >= CONNECT_GROUP_SIZE {
                let group = Self::dense_group_columns(best_nodes, &columns, next);
                Self::connect_group_with::<false>(
                    best_nodes, later_ends, site, next, group, overflow,
                );
                next -= CONNECT_GROUP_SIZE;
                continue;
            }
            next -= 1;
            let column = columns.right_column(best_nodes[next].candidate.left_id);
            let (cost, best) = best_left::<false>(site.lefts, |id| {
                debug_assert!(usize::from(id) < column.len());
                unsafe { *column.get_unchecked(usize::from(id)) }
            });
            store_best::<false>(
                best_nodes,
                later_ends,
                site.position,
                next,
                cost,
                best,
                overflow,
            );
        }
    }

    #[inline(always)]
    fn dense_group_columns<'m>(
        best_nodes: &[BestNode],
        columns: &DenseColumns<'m>,
        next: usize,
    ) -> [&'m [i16]; CONNECT_GROUP_SIZE] {
        std::array::from_fn(|lane| {
            columns.right_column(best_nodes[next - 1 - lane].candidate.left_id)
        })
    }

    #[inline(always)]
    fn connect_best_hot(
        best_nodes: &mut [BestNode],
        later_ends: &mut [EndSlot],
        site: &BestSite<'_>,
        hot: &HotView<'_>,
        overflow: &mut bool,
    ) {
        let BestSite {
            lefts,
            widest,
            position,
            first,
        } = *site;
        #[cfg(feature = "pair-stats")]
        record_pairs(&best_nodes[first..], lefts);
        let reaches = |cells: &[i16]| cells.len() > usize::from(widest);
        prefetch_columns(&best_nodes[first..], lefts, hot, reaches);
        let mut next = best_nodes.len();
        while next > first {
            if next - first >= CONNECT_GROUP_SIZE
                && Self::connect_group(best_nodes, later_ends, site, hot, next, reaches, overflow)
            {
                next -= CONNECT_GROUP_SIZE;
                continue;
            }
            next -= 1;
            let column = hot.right_column(best_nodes[next].candidate.left_id);
            let cells = column.cells();
            let (cost, best) = if reaches(cells) {
                let cost = |id: u16| unsafe { *cells.get_unchecked(usize::from(id)) };
                best_left::<true>(lefts, cost)
            } else {
                best_left::<true>(lefts, column.into_cost_fn())
            };
            store_best::<true>(best_nodes, later_ends, position, next, cost, best, overflow);
        }
    }

    #[inline(always)]
    fn connect_group(
        best_nodes: &mut [BestNode],
        later_ends: &mut [EndSlot],
        site: &BestSite<'_>,
        hot: &HotView<'_>,
        next: usize,
        reaches: impl Fn(&[i16]) -> bool,
        overflow: &mut bool,
    ) -> bool {
        let columns: [&[i16]; CONNECT_GROUP_SIZE] = std::array::from_fn(|lane| {
            hot.right_column(best_nodes[next - 1 - lane].candidate.left_id)
                .cells()
        });
        if !columns.iter().all(|cells| reaches(cells)) {
            return false;
        }
        Self::connect_group_with::<true>(best_nodes, later_ends, site, next, columns, overflow);
        true
    }

    #[inline(always)]
    fn connect_group_with<const TRACK_WIDEST: bool>(
        best_nodes: &mut [BestNode],
        later_ends: &mut [EndSlot],
        site: &BestSite<'_>,
        next: usize,
        columns: [&[i16]; CONNECT_GROUP_SIZE],
        overflow: &mut bool,
    ) {
        let found = best_lefts(site.lefts, columns);
        for (lane, (cost, best)) in found.into_iter().enumerate() {
            store_best::<TRACK_WIDEST>(
                best_nodes,
                later_ends,
                site.position,
                next - 1 - lane,
                cost,
                best,
                overflow,
            );
        }
    }
}
