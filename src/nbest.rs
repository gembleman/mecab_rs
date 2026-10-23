use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::ids::{ElementId, LinkId, NodeId};

#[derive(Clone, Copy)]
pub(super) struct PathLink {
    pub(super) left_node: NodeId,
    pub(super) cost: i32,
    pub(super) next_left: Option<LinkId>,
}

#[derive(Clone, Copy)]
struct Element {
    node: NodeId,
    next: Option<ElementId>,
    path_cost: i64,
}

type AgendaKey = Reverse<(i64, usize)>;

#[derive(Default)]
pub(super) struct Agenda {
    elements: Vec<Element>,
    heap: BinaryHeap<AgendaKey>,
}

impl Agenda {
    pub(super) fn start(&mut self, eos: NodeId) {
        self.elements.clear();
        self.heap.clear();
        self.elements.push(Element { node: eos, next: None, path_cost: 0 });
        self.heap.push(Reverse((0, 0)));
    }

    pub(super) fn node(&self, element: ElementId) -> NodeId {
        self.elements[element].node
    }

    pub(super) fn next(&self, element: ElementId) -> Option<ElementId> {
        self.elements[element].next
    }

    pub(super) fn path_cost(&self, element: ElementId) -> i64 {
        self.elements[element].path_cost
    }

    pub(super) fn extend(&mut self, top: ElementId, node: NodeId, node_cost: i32, step_cost: i32) {
        let path_cost = i64::from(step_cost) + self.path_cost(top);
        let sequence = self.elements.len();
        self.elements.push(Element { node, next: Some(top), path_cost });
        let estimate = i64::from(node_cost) + path_cost;
        self.heap.push(Reverse((estimate, sequence)));
    }

    pub(super) fn pop(&mut self) -> Option<ElementId> {
        let Reverse((_, sequence)) = self.heap.pop()?;
        Some(ElementId::from_index(sequence))
    }
}
