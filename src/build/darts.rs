
#[derive(Clone, Copy, Default)]
struct Unit {
    base: i32,
    check: u32,
}

struct Node {
    code: u32,
    depth: usize,
    left: usize,
    right: usize,
}

struct Builder<'a> {
    array: Vec<Unit>,
    used: Vec<u8>,
    size: usize,
    keys: &'a [&'a [u8]],
    values: &'a [i32],
    progress: usize,
    next_check_pos: usize,
}

impl Builder<'_> {
    fn resize(&mut self, length: usize) {
        if length > self.array.len() {
            self.array.resize(length, Unit::default());
            self.used.resize(length, 0);
        }
    }

    fn fetch(&self, parent: &Node) -> Vec<Node> {
        let mut siblings: Vec<Node> = Vec::new();
        let mut previous = 0;
        for index in parent.left..parent.right {
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
                    last.right = index;
                }
                siblings.push(Node {
                    code: current,
                    depth: parent.depth + 1,
                    left: index,
                    right: 0,
                });
            }
            previous = current;
        }
        if let Some(last) = siblings.last_mut() {
            last.right = parent.right;
        }
        siblings
    }

    fn insert(&mut self, siblings: &[Node]) -> usize {
        let first_code = siblings[0].code as usize;
        let last_code = siblings[siblings.len() - 1].code as usize;
        let mut begin;
        let mut position = (first_code + 1).max(self.next_check_pos) - 1;
        let mut nonzero = 0usize;
        let mut first = false;
        self.resize(position + 1);

        'search: loop {
            position += 1;
            self.resize(position + 1);
            if self.array[position].check != 0 {
                nonzero += 1;
                continue;
            } else if !first {
                self.next_check_pos = position;
                first = true;
            }
            begin = position - first_code;
            if self.array.len() <= begin + last_code {
                let length = self.array.len();
                let factor = if self.progress == 0 {
                    1.05
                } else {
                    (self.keys.len() as f64 / self.progress as f64).clamp(1.05, 2.0)
                };
                self.resize(((length as f64 * factor) as usize).max(begin + last_code + 1));
            }
            if self.used[begin] != 0 {
                continue;
            }
            for sibling in &siblings[1..] {
                if self.array[begin + sibling.code as usize].check != 0 {
                    continue 'search;
                }
            }
            break;
        }

        if nonzero as f64 / (position.wrapping_sub(self.next_check_pos).wrapping_add(1)) as f64
            >= 0.95
        {
            self.next_check_pos = position;
        }

        self.used[begin] = 1;
        self.size = self.size.max(begin + last_code + 1);
        for sibling in siblings {
            self.array[begin + sibling.code as usize].check = begin as u32;
        }
        for sibling in siblings {
            let children = self.fetch(sibling);
            let at = begin + sibling.code as usize;
            if children.is_empty() {
                self.array[at].base = -self.values[sibling.left] - 1;
                self.progress += 1;
            } else {
                let child_begin = self.insert(&children);
                self.array[at].base = child_begin as i32;
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
        used: Vec::new(),
        size: 0,
        keys,
        values,
        progress: 0,
        next_check_pos: 0,
    };
    builder.resize(8192);
    builder.array[0].base = 1;
    let root = Node {
        code: 0,
        depth: 0,
        left: 0,
        right: keys.len(),
    };
    let siblings = builder.fetch(&root);
    builder.insert(&siblings);

    let size = builder.size + 256 + 1;
    builder.resize(size);
    let mut bytes = Vec::with_capacity(size * 8);
    for unit in &builder.array[..size] {
        bytes.extend_from_slice(&unit.base.to_le_bytes());
        bytes.extend_from_slice(&unit.check.to_le_bytes());
    }
    bytes
}
