use std::ops::Range;

use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::ids::NodeId;
use crate::matrix::Matrix;
use crate::nbest::{LinkSpan, PathLink};

use super::cost::{best_left, cost_in_range, links_fit};
use super::{Lattice, LeftCandidate};

fn newest_first<I: DoubleEndedIterator>(candidates: I) -> std::iter::Rev<I> {
    candidates.rev()
}

impl Lattice {
    pub(super) fn connect<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        position: usize,
        begun: Range<usize>,
    ) -> Result<(), Error> {
        if NBEST {
            let added = begun.len().saturating_mul(self.ends[position].lefts.len());
            if !links_fit(self.links.len(), added) {
                return Err(Error::TextTooLong);
            }
        }
        let mut lefts = std::mem::take(&mut self.ends[position]);
        self.connect_rights::<NBEST>(dictionary, position, begun, &lefts.lefts, lefts.widest);
        if self.cost_overflow {
            return Err(Error::TextTooLong);
        }
        let added = &mut self.ends[position];
        if !added.lefts.is_empty() {
            lefts.absorb(added);
        }
        std::mem::swap(added, &mut lefts);
        Ok(())
    }

    fn connect_rights<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        position: usize,
        begun: Range<usize>,
        lefts: &[LeftCandidate],
        widest: u16,
    ) {
        match &dictionary.matrix {
            Matrix::PackedDense(dense) => {
                for right_index in newest_first(begun) {
                    let right_id = NodeId::from_index(right_index);
                    let column = dense.right_column(self.nodes[right_id].left_id);
                    self.connect_right::<NBEST, false>(
                        position,
                        right_id,
                        lefts,
                        |left_right_id| {
                            debug_assert!(usize::from(left_right_id) < column.len());
                            unsafe { *column.get_unchecked(usize::from(left_right_id)) }
                        },
                    );
                }
            }
            Matrix::PackedHot(hot) => {
                let hot = hot.view();
                for right_index in newest_first(begun) {
                    let right_id = NodeId::from_index(right_index);
                    let column = hot.right_column(self.nodes[right_id].left_id);
                    let cells = column.cells();
                    if cells.len() > usize::from(widest) {
                        let cost = |left_right_id: u16| unsafe {
                            *cells.get_unchecked(usize::from(left_right_id))
                        };
                        self.connect_right::<NBEST, true>(position, right_id, lefts, cost);
                    } else {
                        self.connect_right::<NBEST, true>(
                            position,
                            right_id,
                            lefts,
                            column.into_cost_fn(),
                        );
                    }
                }
            }
        }
    }

    fn connect_right<const NBEST: bool, const SELECT: bool>(
        &mut self,
        position: usize,
        right_id: NodeId,
        lefts: &[LeftCandidate],
        mut connection_cost: impl FnMut(u16) -> i16,
    ) {
        let right_node = self.nodes[right_id];
        let word_cost = i32::from(right_node.word_cost);
        let links_start = self.links.len();
        let (best_cost, best) = if NBEST {
            let mut best_cost = i32::MAX;
            let mut best = lefts[0].id;
            for left in newest_first(lefts.iter()) {
                let link_cost = i32::from(connection_cost(left.right_id)) + word_cost;
                let cost = left.cost + link_cost;
                if cost < best_cost {
                    best_cost = cost;
                    best = left.id;
                }
                self.links.push(PathLink {
                    left_node: left.id,
                    cost: link_cost,
                });
            }
            (best_cost, best)
        } else {
            let (best_cost, best) = best_left::<SELECT>(lefts, connection_cost);
            (best_cost + word_cost, best)
        };
        let target = right_node.length_with_space as usize + position;
        self.cost_overflow |= !cost_in_range(best_cost);
        let node = &mut self.nodes[right_id];
        node.prev = Some(best);
        node.cost = best_cost;
        node.links = LinkSpan::new(links_start, self.links.len());
        let ending = LeftCandidate {
            right_id: right_node.right_id,
            cost: best_cost,
            id: right_id,
        };
        self.ends[target].push(ending);
    }
}
