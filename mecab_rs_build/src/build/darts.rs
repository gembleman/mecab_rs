const INITIAL_UNITS: usize = 8192;
const ROOT_BASE: i32 = 1;
const TERMINAL_PADDING_UNITS: usize = 256 + 1;
const UNIT_BYTES: usize = 8;
const MIN_GROWTH_FACTOR: f64 = 1.05;
const MAX_GROWTH_FACTOR: f64 = 2.0;
const DENSE_SCAN_RATIO: f64 = 0.95;

#[derive(Clone, Copy, Default)]
struct Unit {
    base: i32,
    check: u32,
}

struct TrieNode {
    label: u32,
    depth: usize,
    first_key: usize,
    end_key: usize,
}

struct BaseSearch {
    begin: usize,
    candidate_cell: usize,
    taken_cells_skipped: usize,
}

struct Frame {
    siblings: Vec<TrieNode>,
    begin: usize,
    next_sibling: usize,
    parent_cell: Option<usize>,
}

struct Builder<'a> {
    array: Vec<Unit>,
    begin_taken: Vec<u8>,
    used_units: usize,
    keys: &'a [&'a [u8]],
    values: &'a [i32],
    keys_placed: usize,
    next_check_pos: usize,
}

impl Builder<'_> {
    fn grow_to(&mut self, length: usize) {
        if length > self.array.len() {
            self.array.resize(length, Unit::default());
            self.begin_taken.resize(length, 0);
        }
    }

    fn children_of(&self, parent: &TrieNode) -> Vec<TrieNode> {
        let mut siblings: Vec<TrieNode> = Vec::new();
        let mut previous = 0;
        for index in parent.first_key..parent.end_key {
            let key = self.keys[index];
            if key.len() < parent.depth {
                continue;
            }
            let current = if key.len() == parent.depth {
                0
            } else {
                u32::from(key[parent.depth]) + 1
            };
            debug_assert!(previous <= current, "the keys are sorted");
            if current != previous || siblings.is_empty() {
                if let Some(last) = siblings.last_mut() {
                    last.end_key = index;
                }
                siblings.push(TrieNode {
                    label: current,
                    depth: parent.depth + 1,
                    first_key: index,
                    end_key: 0,
                });
            }
            previous = current;
        }
        if let Some(last) = siblings.last_mut() {
            last.end_key = parent.end_key;
        }
        siblings
    }

    fn grow_for_base(&mut self, begin: usize, last_label: usize) {
        if self.array.len() <= begin + last_label {
            let length = self.array.len();
            let factor = if self.keys_placed == 0 {
                MIN_GROWTH_FACTOR
            } else {
                (self.keys.len() as f64 / self.keys_placed as f64)
                    .clamp(MIN_GROWTH_FACTOR, MAX_GROWTH_FACTOR)
            };
            self.grow_to(((length as f64 * factor) as usize).max(begin + last_label + 1));
        }
    }

    fn find_base(&mut self, siblings: &[TrieNode]) -> BaseSearch {
        let first_label = siblings[0].label as usize;
        let last_label = siblings[siblings.len() - 1].label as usize;
        let mut candidate_cell = (first_label + 1).max(self.next_check_pos) - 1;
        let mut taken_cells_skipped = 0usize;
        let mut found_first_free = false;
        self.grow_to(candidate_cell + 1);

        'search: loop {
            candidate_cell += 1;
            self.grow_to(candidate_cell + 1);
            if self.array[candidate_cell].check != 0 {
                taken_cells_skipped += 1;
                continue;
            } else if !found_first_free {
                self.next_check_pos = candidate_cell;
                found_first_free = true;
            }
            let begin = candidate_cell - first_label;
            self.grow_for_base(begin, last_label);
            if self.begin_taken[begin] != 0 {
                continue;
            }
            for sibling in &siblings[1..] {
                if self.array[begin + sibling.label as usize].check != 0 {
                    continue 'search;
                }
            }
            return BaseSearch {
                begin,
                candidate_cell,
                taken_cells_skipped,
            };
        }
    }

    fn commit_base(&mut self, siblings: &[TrieNode], search: &BaseSearch) {
        let begin = search.begin;
        let last_label = siblings[siblings.len() - 1].label as usize;
        let scanned_since_first_free = search
            .candidate_cell
            .wrapping_sub(self.next_check_pos)
            .wrapping_add(1);
        if search.taken_cells_skipped as f64 / scanned_since_first_free as f64 >= DENSE_SCAN_RATIO {
            self.next_check_pos = search.candidate_cell;
        }
        self.begin_taken[begin] = 1;
        self.used_units = self.used_units.max(begin + last_label + 1);
        for sibling in siblings {
            self.array[begin + sibling.label as usize].check = begin as u32;
        }
    }

    fn open_frame(&mut self, siblings: Vec<TrieNode>, parent_cell: Option<usize>) -> Frame {
        let search = self.find_base(&siblings);
        self.commit_base(&siblings, &search);
        Frame {
            siblings,
            begin: search.begin,
            next_sibling: 0,
            parent_cell,
        }
    }

    fn place_siblings(&mut self, siblings: Vec<TrieNode>) {
        let first = self.open_frame(siblings, None);
        let mut stack = vec![first];
        while let Some(frame) = stack.last_mut() {
            let Some(sibling) = frame.siblings.get(frame.next_sibling) else {
                let finished = stack.pop().expect("the stack is not empty");
                if let Some(cell) = finished.parent_cell {
                    self.array[cell].base = finished.begin as i32;
                }
                continue;
            };
            let cell = frame.begin + sibling.label as usize;
            let first_key = sibling.first_key;
            let children = self.children_of(sibling);
            frame.next_sibling += 1;
            if children.is_empty() {
                self.array[cell].base = -self.values[first_key] - 1;
                self.keys_placed += 1;
            } else {
                let child = self.open_frame(children, Some(cell));
                stack.push(child);
            }
        }
    }
}

pub(crate) fn build(keys: &[&[u8]], values: &[i32]) -> Vec<u8> {
    debug_assert_eq!(keys.len(), values.len());
    debug_assert!(values.iter().all(|&value| value >= 0));
    debug_assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    if keys.is_empty() {
        return Vec::new();
    }
    let mut builder = Builder {
        array: Vec::new(),
        begin_taken: Vec::new(),
        used_units: 0,
        keys,
        values,
        keys_placed: 0,
        next_check_pos: 0,
    };
    builder.grow_to(INITIAL_UNITS);
    builder.array[0].base = ROOT_BASE;
    let root = TrieNode {
        label: 0,
        depth: 0,
        first_key: 0,
        end_key: keys.len(),
    };
    let siblings = builder.children_of(&root);
    builder.place_siblings(siblings);
    builder.into_bytes()
}

impl Builder<'_> {
    fn into_bytes(mut self) -> Vec<u8> {
        let used_units = self.used_units + TERMINAL_PADDING_UNITS;
        self.grow_to(used_units);
        let mut bytes = Vec::with_capacity(used_units * UNIT_BYTES);
        for unit in &self.array[..used_units] {
            bytes.extend_from_slice(&unit.base.to_le_bytes());
            bytes.extend_from_slice(&unit.check.to_le_bytes());
        }
        bytes
    }
}
