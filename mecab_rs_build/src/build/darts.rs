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

    fn place_siblings(&mut self, siblings: &[TrieNode]) -> usize {
        let first_label = siblings[0].label as usize;
        let last_label = siblings[siblings.len() - 1].label as usize;
        let mut begin;
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
            begin = candidate_cell - first_label;
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
            if self.begin_taken[begin] != 0 {
                continue;
            }
            for sibling in &siblings[1..] {
                if self.array[begin + sibling.label as usize].check != 0 {
                    continue 'search;
                }
            }
            break;
        }

        let scanned_since_first_free = candidate_cell
            .wrapping_sub(self.next_check_pos)
            .wrapping_add(1);
        if taken_cells_skipped as f64 / scanned_since_first_free as f64 >= DENSE_SCAN_RATIO {
            self.next_check_pos = candidate_cell;
        }

        self.begin_taken[begin] = 1;
        self.used_units = self.used_units.max(begin + last_label + 1);
        for sibling in siblings {
            self.array[begin + sibling.label as usize].check = begin as u32;
        }
        for sibling in siblings {
            let children = self.children_of(sibling);
            let cell = begin + sibling.label as usize;
            if children.is_empty() {
                self.array[cell].base = -self.values[sibling.first_key] - 1;
                self.keys_placed += 1;
            } else {
                let child_begin = self.place_siblings(&children);
                self.array[cell].base = child_begin as i32;
            }
        }
        begin
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
    builder.place_siblings(&siblings);

    let used_units = builder.used_units + TERMINAL_PADDING_UNITS;
    builder.grow_to(used_units);
    let mut bytes = Vec::with_capacity(used_units * UNIT_BYTES);
    for unit in &builder.array[..used_units] {
        bytes.extend_from_slice(&unit.base.to_le_bytes());
        bytes.extend_from_slice(&unit.check.to_le_bytes());
    }
    bytes
}
