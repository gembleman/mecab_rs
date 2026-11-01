use std::fmt;
use std::slice;

use super::{Frame, Node};
use crate::ids::NodeId;
use crate::lattice::RawNode;

pub struct Path<'t, 'x> {
    pub(super) frame: Frame<'t, 'x>,
    pub(super) nodes: &'t [RawNode],
    pub(super) order: slice::Iter<'t, NodeId>,
    pub(super) cost: i64,
}

impl fmt::Debug for Path<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Path")
            .field("text", &self.frame.text)
            .field("cost", &self.cost)
            .field("remaining", &self.order.len())
            .finish_non_exhaustive()
    }
}

impl Path<'_, '_> {
    pub fn cost(&self) -> i64 {
        self.cost
    }
}

impl<'t, 'x> Iterator for Path<'t, 'x> {
    type Item = Node<'t, 'x>;

    fn next(&mut self) -> Option<Node<'t, 'x>> {
        let &id = self.order.next()?;
        Some(Node {
            frame: self.frame,
            raw: &self.nodes[id],
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.order.size_hint()
    }
}

impl ExactSizeIterator for Path<'_, '_> {}
