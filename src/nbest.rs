pub(super) const NIL: u32 = u32::MAX;

#[derive(Clone, Copy)]
pub(super) struct PathLink {
    pub(super) left_node: u32,
    pub(super) cost: i32,
    pub(super) next_left: u32,
}

#[derive(Clone, Copy)]
struct Element {
    node: u32,
    next: u32,
    path_cost: i64,
}

#[derive(Clone, Copy)]
struct Priority {
    estimate: i64,
    element: u32,
}

impl Priority {
    fn is_worse_than(self, other: Priority) -> bool {
        self.estimate > other.estimate
    }
}

#[derive(Default)]
pub(super) struct Agenda {
    elements: Vec<Element>,
    heap: Vec<Priority>,
}

impl Agenda {
    pub(super) fn start(&mut self, eos: u32) {
        self.elements.clear();
        self.heap.clear();
        self.elements.push(Element { node: eos, next: NIL, path_cost: 0 });
        self.heap.push(Priority { estimate: 0, element: 0 });
    }

    pub(super) fn node(&self, element: u32) -> u32 {
        self.elements[element as usize].node
    }

    pub(super) fn next(&self, element: u32) -> u32 {
        self.elements[element as usize].next
    }

    pub(super) fn path_cost(&self, element: u32) -> i64 {
        self.elements[element as usize].path_cost
    }

    pub(super) fn extend(&mut self, top: u32, node: u32, node_cost: i32, step_cost: i32) {
        let path_cost = i64::from(step_cost) + self.path_cost(top);
        self.elements.push(Element { node, next: top, path_cost });
        let element = (self.elements.len() - 1) as u32;
        let estimate = i64::from(node_cost) + path_cost;
        self.push_heap(Priority { estimate, element });
    }

    pub(super) fn pop(&mut self) -> Option<u32> {
        let remaining = self.heap.len().checked_sub(1)?;
        let top = self.heap[0].element;
        let value = self.heap[remaining];
        self.heap.truncate(remaining);
        if remaining > 0 {
            self.sift_down_from_root(value);
        }
        Some(top)
    }

    fn push_heap(&mut self, value: Priority) {
        self.heap.push(value);
        let hole = self.heap.len() - 1;
        self.sift_up(hole, value);
    }

    fn sift_up(&mut self, mut hole: usize, value: Priority) {
        while hole > 0 {
            let parent = (hole - 1) / 2;
            if !self.heap[parent].is_worse_than(value) {
                break;
            }
            self.heap[hole] = self.heap[parent];
            hole = parent;
        }
        self.heap[hole] = value;
    }

    fn sift_down_from_root(&mut self, value: Priority) {
        let length = self.heap.len();
        let mut hole = 0;
        let mut child = 0;
        while child < (length - 1) / 2 {
            child = 2 * (child + 1);
            if self.heap[child].is_worse_than(self.heap[child - 1]) {
                child -= 1;
            }
            self.heap[hole] = self.heap[child];
            hole = child;
        }
        if length.is_multiple_of(2) && child == (length - 2) / 2 {
            child = 2 * (child + 1);
            self.heap[hole] = self.heap[child - 1];
            hole = child - 1;
        }
        self.sift_up(hole, value);
    }
}
