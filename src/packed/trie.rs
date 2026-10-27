
use super::{
    COUNT_SHIFT, INLINE_COUNT_MASK, INLINE_FLAG, MAX_UNITS, PARENT_MASK, TERMINAL_FLAG,
    UNUSED_CHECK,
};
use crate::Error;

const NONE: u32 = u32::MAX;
const FRONT_TRIES: usize = 6000;
const TOO_BIG: &str = "the double array needs more than 16 million units";

pub(super) struct Entry<'a> {
    pub(super) codes: &'a [u16],
    pub(super) first_token: u32,
    pub(super) count: u8,
}

struct Node {
    first_child: u32,
    last_child: u32,
    next_sibling: u32,
    children: u32,
    code: u16,
    count: u8,
    first_token: u32,
    position: u32,
}

impl Node {
    fn new(code: u16) -> Self {
        Self {
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
            children: 0,
            code,
            count: 0,
            first_token: 0,
            position: 0,
        }
    }

    fn is_terminal(&self) -> bool {
        self.count > 0
    }

    fn is_inline(&self) -> bool {
        self.is_terminal() && self.children == 0 && u32::from(self.count) <= INLINE_COUNT_MASK
    }

    fn needs_data_unit(&self) -> bool {
        self.is_terminal() && !self.is_inline()
    }
}

struct Units {
    cells: Vec<[u32; 2]>,
    skip: Vec<u32>,
    cursor: usize,
}

impl Units {
    fn is_free(&self, index: usize) -> bool {
        index >= self.cells.len() || self.cells[index][1] == UNUSED_CHECK
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
            self.cells.resize(length, [0, UNUSED_CHECK]);
            self.skip.extend(from..length as u32);
        }
        Ok(())
    }

    fn take(&mut self, index: usize, unit: [u32; 2]) {
        self.cells[index] = unit;
        self.skip[index] = index as u32 + 1;
    }
}

pub(super) fn build(entries: &[Entry]) -> Result<Vec<[u32; 2]>, Error> {
    let mut nodes = vec![Node::new(0)];
    link_trie(entries, &mut nodes)?;
    let mut units = Units {
        cells: Vec::new(),
        skip: Vec::new(),
        cursor: 0,
    };
    units.grow(1)?;
    units.take(0, [0, PARENT_MASK]);
    let mut queue = vec![0u32];
    let mut codes: Vec<u16> = Vec::new();
    let mut children: Vec<u32> = Vec::new();
    let mut at = 0;
    while at < queue.len() {
        let id = queue[at] as usize;
        at += 1;
        codes.clear();
        children.clear();
        if nodes[id].needs_data_unit() {
            codes.push(0);
        }
        let mut child = nodes[id].first_child;
        while child != NONE {
            codes.push(nodes[child as usize].code);
            children.push(child);
            child = nodes[child as usize].next_sibling;
        }
        if codes.is_empty() {
            continue;
        }
        let base = find_base(&mut units, &codes)?;
        let parent = nodes[id].position;
        units.cells[parent as usize][0] = base as u32;
        if nodes[id].needs_data_unit() {
            let node = &nodes[id];
            let data = [
                node.first_token | u32::from(node.count) << COUNT_SHIFT,
                parent,
            ];
            units.take(base, data);
        }
        for &child in &children {
            let node = &mut nodes[child as usize];
            let position = base + usize::from(node.code);
            node.position = position as u32;
            let (value, flags) = if node.is_inline() {
                let count = u32::from(node.count) << COUNT_SHIFT;
                (node.first_token, TERMINAL_FLAG | INLINE_FLAG | count)
            } else if node.is_terminal() {
                (0, TERMINAL_FLAG)
            } else {
                (0, 0)
            };
            units.take(position, [value, parent | flags]);
            queue.push(child);
        }
    }
    Ok(units.cells)
}

fn link_trie(entries: &[Entry], nodes: &mut Vec<Node>) -> Result<(), Error> {
    let mut path: Vec<u32> = vec![0];
    let mut previous: &[u16] = &[];
    for entry in entries {
        let codes = entry.codes;
        let shared = codes
            .iter()
            .zip(previous)
            .take_while(|(a, b)| a == b)
            .count();
        if entry.first_token > PARENT_MASK {
            return Err(Error::Unsupported("more than 16 million tokens"));
        }
        if codes.is_empty() || shared == codes.len() || entry.count == 0 {
            return Err(Error::Unsupported("an empty, repeated or tokenless key"));
        }
        path.truncate(shared + 1);
        for &code in &codes[shared..] {
            let id = nodes.len() as u32;
            let parent = *path.last().expect("the root stays on the path") as usize;
            nodes.push(Node::new(code));
            if nodes[parent].first_child == NONE {
                nodes[parent].first_child = id;
            } else {
                let last = nodes[parent].last_child as usize;
                nodes[last].next_sibling = id;
            }
            nodes[parent].last_child = id;
            nodes[parent].children += 1;
            path.push(id);
        }
        let node = &mut nodes[*path.last().expect("the path is not empty") as usize];
        node.count = entry.count;
        node.first_token = entry.first_token;
        previous = codes;
    }
    Ok(())
}

fn find_base(units: &mut Units, codes: &[u16]) -> Result<usize, Error> {
    let first = usize::from(codes[0]);
    let mut candidate = units.next_free(first);
    let mut tries = 0;
    let mut jumped = false;
    let base = loop {
        let base = candidate - first;
        if codes[1..]
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
