use super::{
    DATA_LABEL, INLINE_COUNT_MASK, INLINE_COUNT_SHIFT, INLINE_FLAG, MAX_UNITS, ROOT_LABEL,
    TERMINAL_FLAG, UNUSED_LABEL, Unit, VALUE_MASK, unit,
};
use crate::Error;
use std::collections::VecDeque;

const BITS_PER_WORD: usize = 64;
const FRONT_TRIES: usize = 6000;
const UNIT_SLACK_DIVISOR: usize = 3;
const TOO_BIG: &str = "the double array needs more than 16 million units";
const FREE: u64 = unit(0, UNUSED_LABEL, 0);

pub(super) struct Entry<'a> {
    pub(super) codes: &'a [u16],
    pub(super) key: u32,
    pub(super) count: u8,
}

pub(super) struct Placed {
    cells: Vec<u64>,
}

impl Placed {
    pub(super) fn finish(mut self, first_tokens: &[u32]) -> Result<Vec<u64>, Error> {
        for cell in &mut self.cells {
            let unit = Unit::from_bits(*cell);
            if unit.label() != DATA_LABEL && !unit.is_inline() {
                continue;
            }
            let first_token = first_tokens[unit.value() as usize];
            if u64::from(first_token) > VALUE_MASK {
                return Err(Error::Unsupported("more than 16 million tokens"));
            }
            *cell = *cell & !VALUE_MASK | u64::from(first_token);
        }
        Ok(self.cells)
    }
}

pub(super) struct Linked {
    nodes: Vec<Node>,
}

struct Node {
    end: u32,
    key: u32,
    code: u16,
    count: u8,
}

impl Node {
    fn new(code: u16) -> Self {
        Self {
            end: 0,
            key: 0,
            code,
            count: 0,
        }
    }

    fn is_terminal(&self) -> bool {
        self.count > 0
    }

    fn is_leaf(&self, id: u32) -> bool {
        self.end == id + 1
    }

    fn is_inline(&self, id: u32) -> bool {
        self.is_terminal() && self.is_leaf(id) && self.count <= INLINE_COUNT_MASK
    }

    fn needs_data_unit(&self, id: u32) -> bool {
        self.is_terminal() && !self.is_inline(id)
    }
}

struct TakenBases {
    words: Vec<u64>,
}

impl TakenBases {
    fn with_capacity(bases: usize) -> Self {
        Self {
            words: Vec::with_capacity(bases / BITS_PER_WORD + 1),
        }
    }

    fn contains(&self, base: usize) -> bool {
        self.words
            .get(base / BITS_PER_WORD)
            .is_some_and(|word| word >> (base % BITS_PER_WORD) & 1 != 0)
    }

    fn insert(&mut self, base: usize) {
        let word = base / BITS_PER_WORD;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1 << (base % BITS_PER_WORD);
    }
}

struct Units {
    cells: Vec<u64>,
    skip: Vec<u32>,
    bases: TakenBases,
    cursor: usize,
}

impl Units {
    fn is_free(&self, index: usize) -> bool {
        index >= self.cells.len() || Unit::from_bits(self.cells[index]).label() == UNUSED_LABEL
    }

    fn base_is_taken(&self, base: usize) -> bool {
        self.bases.contains(base)
    }

    fn take_base(&mut self, base: usize) {
        self.bases.insert(base);
    }

    fn set_value(&mut self, index: usize, value: u32) {
        self.cells[index] = self.cells[index] & !VALUE_MASK | u64::from(value);
    }

    fn next_free(&mut self, index: usize) -> usize {
        let length = self.cells.len();
        let mut root = index;
        while root < length && self.skip[root] as usize != root {
            root = self.skip[root] as usize;
        }
        let mut walk = index;
        while walk < length && self.skip[walk] as usize != walk {
            let next = self.skip[walk] as usize;
            self.skip[walk] = root as u32;
            walk = next;
        }
        root
    }

    fn grow(&mut self, length: usize) -> Result<(), Error> {
        if length > MAX_UNITS {
            return Err(Error::Unsupported(TOO_BIG));
        }
        if length > self.cells.len() {
            let from = self.cells.len() as u32;
            self.cells.resize(length, FREE);
            self.skip.extend(from..length as u32);
        }
        Ok(())
    }

    fn take(&mut self, index: usize, unit: u64) {
        self.cells[index] = unit;
        self.skip[index] = index as u32 + 1;
    }
}

fn collect_children(nodes: &[Node], id: u32, codes: &mut Vec<u16>, children: &mut Vec<u32>) {
    codes.clear();
    children.clear();
    let node = &nodes[id as usize];
    if node.needs_data_unit(id) {
        codes.push(0);
    }
    let mut child = id + 1;
    while child < node.end {
        codes.push(nodes[child as usize].code);
        children.push(child);
        child = nodes[child as usize].end;
    }
}

fn child_unit(node: &Node, id: u32) -> u64 {
    if node.is_inline(id) {
        let flags = TERMINAL_FLAG | INLINE_FLAG | node.count << INLINE_COUNT_SHIFT;
        unit(node.key, node.code, flags)
    } else if node.is_terminal() {
        unit(0, node.code, TERMINAL_FLAG)
    } else {
        unit(0, node.code, 0)
    }
}

fn place_children(
    units: &mut Units,
    nodes: &[Node],
    (id, position): (u32, u32),
    base: usize,
    children: &[u32],
    queue: &mut VecDeque<(u32, u32)>,
) {
    units.set_value(position as usize, base as u32);
    let node = &nodes[id as usize];
    if node.needs_data_unit(id) {
        units.take(base, unit(node.key, DATA_LABEL, node.count));
    }
    for &child in children {
        let node = &nodes[child as usize];
        let position = base + usize::from(node.code);
        units.take(position, child_unit(node, child));
        queue.push_back((child, position as u32));
    }
}

fn node_count<'a>(entries: impl Iterator<Item = Entry<'a>>) -> usize {
    let mut previous: &[u16] = &[];
    let mut count = 1;
    for entry in entries {
        let shared = entry
            .codes
            .iter()
            .zip(previous)
            .take_while(|(a, b)| a == b)
            .count();
        count += entry.codes.len().saturating_sub(shared);
        previous = entry.codes;
    }
    count
}

pub(super) fn link<'a>(entries: impl Iterator<Item = Entry<'a>> + Clone) -> Result<Linked, Error> {
    let mut nodes = Vec::with_capacity(node_count(entries.clone()));
    nodes.push(Node::new(0));
    link_trie(entries, &mut nodes)?;
    Ok(Linked { nodes })
}

impl Linked {
    pub(super) fn place(self) -> Result<Placed, Error> {
        let nodes = self.nodes;
        let expected_units = nodes.len() + nodes.len() / UNIT_SLACK_DIVISOR;
        let mut units = Units {
            cells: Vec::with_capacity(expected_units),
            skip: Vec::with_capacity(expected_units),
            bases: TakenBases::with_capacity(expected_units),
            cursor: 0,
        };
        units.grow(1)?;
        units.take(0, unit(0, ROOT_LABEL, 0));
        let mut queue = VecDeque::from([(0u32, 0u32)]);
        let mut codes: Vec<u16> = Vec::new();
        let mut children: Vec<u32> = Vec::new();
        while let Some(front) = queue.pop_front() {
            collect_children(&nodes, front.0, &mut codes, &mut children);
            if codes.is_empty() {
                continue;
            }
            let base = find_base(&mut units, &codes)?;
            units.take_base(base);
            place_children(&mut units, &nodes, front, base, &children, &mut queue);
        }
        units.cells.shrink_to_fit();
        Ok(Placed { cells: units.cells })
    }
}

fn close_path(nodes: &mut [Node], path: &mut Vec<u32>, keep: usize) {
    let end = nodes.len() as u32;
    while path.len() > keep {
        let id = path.pop().expect("the path is longer than keep");
        nodes[id as usize].end = end;
    }
}

fn link_trie<'a>(
    entries: impl Iterator<Item = Entry<'a>>,
    nodes: &mut Vec<Node>,
) -> Result<(), Error> {
    let mut path: Vec<u32> = vec![0];
    let mut previous: &[u16] = &[];
    for entry in entries {
        let codes = entry.codes;
        let shared = codes
            .iter()
            .zip(previous)
            .take_while(|(a, b)| a == b)
            .count();
        if codes.is_empty() || shared == codes.len() || entry.count == 0 {
            return Err(Error::Unsupported("an empty, repeated or tokenless key"));
        }
        if u64::from(entry.key) > VALUE_MASK {
            return Err(Error::Unsupported("more than 16 million keys"));
        }
        close_path(nodes, &mut path, shared + 1);
        for &code in &codes[shared..] {
            path.push(nodes.len() as u32);
            nodes.push(Node::new(code));
        }
        let node = &mut nodes[*path.last().expect("the path is not empty") as usize];
        node.count = entry.count;
        node.key = entry.key;
        previous = codes;
    }
    close_path(nodes, &mut path, 0);
    Ok(())
}

fn find_base(units: &mut Units, codes: &[u16]) -> Result<usize, Error> {
    let first = usize::from(codes[0]);
    let mut candidate = units.next_free(first);
    let mut tries = 0;
    let mut jumped = false;
    let base = loop {
        let base = candidate - first;
        if !units.base_is_taken(base)
            && codes[1..]
                .iter()
                .all(|&code| units.is_free(base + usize::from(code)))
        {
            break base;
        }
        candidate = units.next_free(candidate + 1);
        tries += 1;
        if tries == FRONT_TRIES && codes.len() > 1 {
            candidate = units.next_free(candidate.max(units.cursor));
            jumped = true;
        }
    };
    if jumped {
        units.cursor = candidate;
    }
    let last = usize::from(*codes.last().expect("there is a code"));
    units.grow(base + last + 1)?;
    Ok(base)
}
