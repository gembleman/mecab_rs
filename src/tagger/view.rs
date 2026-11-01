use std::fmt;

use super::{Frame, Node, Path};
use crate::lattice::{Lattice, RawNode};

#[derive(Clone, Copy)]
pub struct LatticeView<'t, 'x> {
    pub(super) frame: Frame<'t, 'x>,
    pub(super) lattice: &'t Lattice,
}

impl fmt::Debug for LatticeView<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LatticeView")
            .field("text", &self.frame.text)
            .field("nodes", &self.lattice.nodes().len())
            .field("best_path_len", &self.lattice.path().len())
            .field("z", &self.z())
            .finish_non_exhaustive()
    }
}

impl<'t, 'x> LatticeView<'t, 'x> {
    fn node(&self, raw: &'t RawNode) -> Node<'t, 'x> {
        Node {
            frame: self.frame,
            raw,
        }
    }

    fn nodes_after_bos(&self) -> &'t [RawNode] {
        &self.lattice.nodes()[1..]
    }

    pub fn bos(&self) -> Node<'t, 'x> {
        debug_assert!(!self.lattice.nodes().is_empty());
        self.node(&self.lattice.nodes()[0])
    }

    pub fn eos(&self) -> Node<'t, 'x> {
        debug_assert!(!self.lattice.nodes().is_empty());
        self.node(
            self.lattice
                .nodes()
                .last()
                .expect("a lattice has an EOS node"),
        )
    }

    pub fn best_path(&self) -> Path<'t, 'x> {
        Path {
            frame: self.frame,
            nodes: self.lattice.nodes(),
            order: self.lattice.path().iter(),
            cost: self.lattice.path_cost(),
        }
    }

    pub fn is_best(&self, node: Node<'_, '_>) -> bool {
        debug_assert!(
            self.lattice.path().is_sorted_by_key(|id| id.index()),
            "the best path is in ascending node order"
        );
        self.index_of(node).is_some_and(|index| {
            self.lattice
                .path()
                .binary_search_by_key(&index, |id| id.index())
                .is_ok()
        })
    }

    fn index_of(&self, node: Node<'_, '_>) -> Option<usize> {
        let nodes = self.lattice.nodes();
        let offset = std::ptr::from_ref(node.raw)
            .addr()
            .checked_sub(nodes.as_ptr().addr())?;
        let index = offset / size_of::<RawNode>();
        (index < nodes.len() && offset.is_multiple_of(size_of::<RawNode>())).then_some(index)
    }

    pub fn prob(&self, node: Node<'_, '_>) -> Option<f32> {
        self.marginal(node).map(|(_, _, prob)| prob)
    }

    pub fn alpha(&self, node: Node<'_, '_>) -> Option<f32> {
        self.marginal(node).map(|(alpha, _, _)| alpha)
    }

    pub fn beta(&self, node: Node<'_, '_>) -> Option<f32> {
        self.marginal(node).map(|(_, beta, _)| beta)
    }

    pub fn z(&self) -> Option<f64> {
        self.lattice.z()
    }

    fn marginal(&self, node: Node<'_, '_>) -> Option<(f32, f32, f32)> {
        self.lattice.marginal(self.index_of(node)?)
    }

    pub fn nodes(&self) -> impl Iterator<Item = Node<'t, 'x>> + use<'t, 'x> {
        let view = *self;
        self.nodes_after_bos()
            .chunk_by(|a, b| a.begin() == b.begin())
            .flat_map(|group| group.iter().rev())
            .map(move |raw| view.node(raw))
    }

    pub fn begin_nodes(
        &self,
        position: usize,
    ) -> impl DoubleEndedIterator<Item = Node<'t, 'x>> + ExactSizeIterator + use<'t, 'x> {
        let after_bos = self.nodes_after_bos();
        let start = after_bos.partition_point(|raw| raw.begin() < position);
        let end = start + after_bos[start..].partition_point(|raw| raw.begin() == position);
        let view = *self;
        after_bos[start..end]
            .iter()
            .rev()
            .map(move |raw| view.node(raw))
    }

    pub fn end_nodes(
        &self,
        position: usize,
    ) -> impl DoubleEndedIterator<Item = Node<'t, 'x>> + ExactSizeIterator + use<'t, 'x> {
        let nodes = self.lattice.nodes();
        let view = *self;
        self.lattice
            .ending_at(position)
            .iter()
            .rev()
            .map(move |left| view.node(&nodes[left.id]))
    }
}
