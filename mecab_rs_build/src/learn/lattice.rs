use std::cmp::Ordering;
use std::io::Write;

use super::features::{FeatureIndex, Features, ID_LIST_END};
use super::surface_and_feature;
use super::tokenizer::{BOS, EOS, Found, NORMAL, Tokenizer, UNKNOWN};
use crate::build::text::{Lines, cstr};
use mecab_rs::Error;

pub(super) const NIL: u32 = u32::MAX;
const TEXT_PADDING: usize = 8;
const WORST_COST: f64 = -1e37;

#[derive(Clone)]
struct Node {
    position: u32,
    length: u32,
    rlength: u32,
    feature: u32,
    fvector: u32,
    bnext: u32,
    enext: u32,
    lpath: u32,
    rpath: u32,
    prev: u32,
    next: u32,
    anext: u32,
    kind: u8,
    char_type: u8,
    alpha: f64,
    beta: f64,
    wcost: f64,
    cost: f64,
}

impl Node {
    fn new(kind: u8) -> Self {
        Self {
            position: 0,
            length: 0,
            rlength: 0,
            feature: 0,
            fvector: NIL,
            bnext: NIL,
            enext: NIL,
            lpath: NIL,
            rpath: NIL,
            prev: NIL,
            next: NIL,
            anext: NIL,
            kind,
            char_type: 0,
            alpha: 0.0,
            beta: 0.0,
            wcost: 0.0,
            cost: 0.0,
        }
    }
}

#[derive(Clone)]
struct Path {
    rnode: u32,
    lnode: u32,
    rnext: u32,
    lnext: u32,
    fvector: u32,
    cost: f64,
}

pub(super) struct Reader<'a> {
    pub(super) tokenizer: &'a mut Tokenizer,
    pub(super) features: &'a mut Features,
    pub(super) index: &'a mut FeatureIndex,
    pub(super) eval_size: usize,
    pub(super) unk_eval_size: usize,
    pub(super) found: Vec<Found>,
}

pub(super) struct Sentence {
    text: Vec<u8>,
    length: usize,
    nodes: Vec<Node>,
    paths: Vec<Path>,
    begin_list: Vec<u32>,
    end_list: Vec<u32>,
    answer: Vec<u32>,
}

fn prefix(feature: &[u8], count: usize) -> &[u8] {
    let mut seen = 0;
    for (at, &byte) in feature.iter().enumerate() {
        if seen == count {
            return &feature[..at];
        }
        if byte == b',' {
            seen += 1;
            if seen == count {
                return &feature[..at];
            }
        }
    }
    feature
}

fn same_node(
    answer_surface: &[u8],
    answer_feature: &[u8],
    node_surface: &[u8],
    node_feature: &[u8],
    node_is_unknown: bool,
    eval_size: usize,
    unk_eval_size: usize,
) -> bool {
    if answer_surface != node_surface {
        return false;
    }
    let size = if node_is_unknown {
        unk_eval_size
    } else {
        eval_size
    };
    prefix(answer_feature, size) == prefix(node_feature, size)
}

impl Sentence {
    fn node(&mut self, node: Node) -> u32 {
        let index = u32::try_from(self.nodes.len()).expect("fewer than 2^32 nodes");
        self.nodes.push(node);
        index
    }

    fn surface(&self, node: u32) -> &[u8] {
        let node = &self.nodes[node as usize];
        let at = node.position as usize;
        &self.text[at..at + node.length as usize]
    }

    fn lookup(&mut self, reader: &mut Reader, position: usize) -> u32 {
        if self.begin_list[position] != NIL {
            return self.begin_list[position];
        }
        reader
            .tokenizer
            .lookup(&self.text, position, self.length, &mut reader.found);
        let mut head = NIL;
        for index in 0..reader.found.len() {
            let found = reader.found[index];
            let feature = reader.tokenizer.feature(found.feature);
            let mut node = Node::new(found.kind);
            node.position = found.position as u32;
            node.length = found.length as u32;
            node.rlength = found.rlength as u32;
            node.char_type = found.char_type;
            node.feature = reader.features.intern(feature);
            node.bnext = head;
            head = self.node(node);
        }
        self.begin_list[position] = head;
        head
    }

    fn connect(&mut self, reader: &mut Reader, position: usize, first: u32) -> Result<(), Error> {
        let mut right = first;
        while right != NIL {
            let mut left = self.end_list[position];
            while left != NIL {
                let path = u32::try_from(self.paths.len()).expect("fewer than 2^32 paths");
                self.paths.push(Path {
                    rnode: right,
                    lnode: left,
                    rnext: self.nodes[left as usize].rpath,
                    lnext: self.nodes[right as usize].lpath,
                    fvector: NIL,
                    cost: 0.0,
                });
                self.nodes[right as usize].lpath = path;
                self.nodes[left as usize].rpath = path;
                self.build_feature(reader, path)?;
                left = self.nodes[left as usize].enext;
            }
            let end = self.nodes[right as usize].rlength as usize + position;
            self.nodes[right as usize].enext = self.end_list[end];
            self.end_list[end] = right;
            right = self.nodes[right as usize].bnext;
        }
        Ok(())
    }

    fn build_feature(&mut self, reader: &mut Reader, path: u32) -> Result<(), Error> {
        let (left, right) = {
            let path = &self.paths[path as usize];
            (path.lnode, path.rnode)
        };
        self.nodes[right as usize].wcost = 0.0;
        self.paths[path as usize].cost = 0.0;
        let left_feature = self.nodes[left as usize].feature;
        let right_feature = self.nodes[right as usize].feature;
        reader.features.rewrite(left_feature)?;
        reader.features.rewrite(right_feature)?;
        let right_node = &self.nodes[right as usize];
        let surface: &[u8] = if right_node.kind == NORMAL {
            let at = right_node.position as usize;
            &self.text[at..at + right_node.length as usize]
        } else {
            b""
        };
        let unigram = reader.index.unigram(
            &reader.features.rewritten(right_feature).unigram,
            right_node.char_type,
            surface,
        )?;
        self.nodes[right as usize].fvector = unigram;
        let bigram = reader.index.bigram(
            &reader.features.rewritten(left_feature).right,
            &reader.features.rewritten(right_feature).left,
        )?;
        self.paths[path as usize].fvector = bigram;
        Ok(())
    }

    fn build_lattice(&mut self, reader: &mut Reader) -> Result<(), Error> {
        for position in 0..=self.length {
            if self.end_list[position] == NIL {
                continue;
            }
            let first = self.lookup(reader, position);
            self.connect(reader, position, first)?;
        }
        if self.end_list[self.length] == NIL {
            self.begin_list[self.length] = self.lookup(reader, self.length);
            for position in (0..=self.length).rev() {
                if self.end_list[position] != NIL {
                    let first = self.begin_list[self.length];
                    self.connect(reader, position, first)?;
                    break;
                }
            }
        }
        Ok(())
    }

    pub(super) fn read(
        reader: &mut Reader,
        lines: &mut Lines,
        observed: &mut Vec<f64>,
        log: &mut dyn Write,
    ) -> Result<Option<Self>, Error> {
        let path = lines.path().to_path_buf();
        let mut words: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut sentence = Vec::new();
        loop {
            let Some(line) = lines.next_line()? else {
                if words.is_empty() {
                    return Ok(None);
                }
                return Err(Error::learn(&path, "missing EOS at the end of the corpus"));
            };
            let text = cstr(line, 0);
            if text == b"EOS" || text.is_empty() {
                break;
            }
            let Some((surface, feature)) = surface_and_feature(text) else {
                return Err(Error::learn(
                    &path,
                    format!("format error: {}", String::from_utf8_lossy(text)),
                ));
            };
            if surface.len() > usize::from(u16::MAX) {
                return Err(Error::learn(&path, "a surface is longer than 65535 bytes"));
            }
            sentence.extend_from_slice(surface);
            words.push((surface.to_vec(), feature.to_vec()));
        }
        if sentence.is_empty() {
            return Err(Error::learn(&path, "empty sentence"));
        }

        let length = sentence.len();
        let mut text = sentence;
        text.extend_from_slice(&[0; TEXT_PADDING]);
        let mut this = Self {
            text,
            length,
            nodes: Vec::new(),
            paths: Vec::new(),
            begin_list: vec![NIL; length + TEXT_PADDING],
            end_list: vec![NIL; length + TEXT_PADDING],
            answer: Vec::new(),
        };
        let bos_feature = reader.features.intern(&reader.tokenizer.bos_feature);
        let mut bos = Node::new(BOS);
        bos.feature = bos_feature;
        let bos = this.node(bos);
        let mut eos = Node::new(EOS);
        eos.feature = bos_feature;
        let eos = this.node(eos);
        this.end_list[0] = bos;
        this.begin_list[length] = eos;

        let mut position = 0;
        for (surface, feature) in &words {
            let mut node = this.lookup(reader, position);
            let mut found = false;
            while node != NIL {
                if this.matches(reader, surface, feature, node) {
                    found = true;
                    break;
                }
                node = this.nodes[node as usize].bnext;
            }
            if !found {
                let mut node = Node::new(NORMAL);
                node.position = position as u32;
                node.length = surface.len() as u32;
                node.rlength = surface.len() as u32;
                node.feature = reader.features.intern(feature);
                node.bnext = this.begin_list[position];
                let node = this.node(node);
                this.begin_list[position] = node;
                let _ = writeln!(
                    log,
                    "adding virtual node: {}",
                    String::from_utf8_lossy(feature)
                );
            }
            position += surface.len();
        }

        this.build_lattice(reader)?;

        let mut prev = this.end_list[0];
        this.nodes[prev as usize].anext = NIL;
        position = 0;
        for index in 0..=words.len() {
            let is_eos = index == words.len();
            let mut right = NIL;
            let mut node = this.begin_list[position];
            while node != NIL {
                if is_eos || this.matches(reader, &words[index].0, &words[index].1, node) {
                    right = node;
                }
                node = this.nodes[node as usize].bnext;
            }
            if right == NIL {
                return Err(Error::learn(&path, "no node for a word of the answer"));
            }
            let mut left_path = NIL;
            let mut candidate = this.nodes[right as usize].lpath;
            while candidate != NIL {
                if this.paths[candidate as usize].lnode == prev {
                    left_path = candidate;
                    break;
                }
                candidate = this.paths[candidate as usize].lnext;
            }
            if left_path == NIL {
                return Err(Error::learn(&path, "lpath is NULL"));
            }

            let pool = reader.index.pool();
            let mut count = |start: u32| {
                let mut at = start as usize;
                while pool[at] != ID_LIST_END {
                    let id = pool[at] as usize;
                    if id >= observed.len() {
                        observed.resize(id + 1, 0.0);
                    }
                    observed[id] += 1.0;
                    at += 1;
                }
            };
            count(this.paths[left_path as usize].fvector);
            if this.nodes[right as usize].kind != EOS {
                count(this.nodes[right as usize].fvector);
            }

            this.answer.push(left_path);
            this.nodes[prev as usize].anext = right;
            prev = right;
            if is_eos {
                break;
            }
            position += words[index].0.len();
        }

        let eos_head = this.begin_list[length];
        this.nodes[prev as usize].anext = eos_head;
        this.nodes[eos_head as usize].anext = NIL;
        Ok(Some(this))
    }

    fn matches(&self, reader: &Reader, surface: &[u8], feature: &[u8], node: u32) -> bool {
        let n = &self.nodes[node as usize];
        same_node(
            surface,
            feature,
            self.surface(node),
            reader.features.get(n.feature),
            n.kind == UNKNOWN,
            reader.eval_size,
            reader.unk_eval_size,
        )
    }

    fn is_empty(&self, path: &Path) -> bool {
        let right = &self.nodes[path.rnode as usize];
        let left = &self.nodes[path.lnode as usize];
        (right.rpath == NIL && right.kind != EOS) || (left.lpath == NIL && left.kind != BOS)
    }

    fn viterbi(&mut self, pool: &[i32], alpha: &[f64]) {
        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NIL {
                let mut wcost = 0.0;
                if self.nodes[node as usize].kind != EOS {
                    let mut at = self.nodes[node as usize].fvector as usize;
                    while pool[at] != ID_LIST_END {
                        wcost += alpha[pool[at] as usize];
                        at += 1;
                    }
                }
                self.nodes[node as usize].wcost = wcost;

                let mut best_cost = WORST_COST;
                let mut best = NIL;
                let mut path = self.nodes[node as usize].lpath;
                while path != NIL {
                    if !self.is_empty(&self.paths[path as usize]) {
                        let record = &self.paths[path as usize];
                        let mut cost = self.nodes[record.rnode as usize].wcost;
                        let mut at = record.fvector as usize;
                        while pool[at] != ID_LIST_END {
                            cost += alpha[pool[at] as usize];
                            at += 1;
                        }
                        self.paths[path as usize].cost = cost;
                    }
                    let record = &self.paths[path as usize];
                    let cost = record.cost + self.nodes[record.lnode as usize].cost;
                    if cost > best_cost {
                        best_cost = cost;
                        best = record.lnode;
                    }
                    path = record.lnext;
                }
                let node_ref = &mut self.nodes[node as usize];
                node_ref.prev = best;
                node_ref.cost = best_cost;
                node = node_ref.bnext;
            }
        }

        let mut node = self.begin_list[self.length];
        while self.nodes[node as usize].prev != NIL {
            let prev = self.nodes[node as usize].prev;
            self.nodes[prev as usize].next = node;
            node = prev;
        }
    }

    fn forward(&mut self) {
        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NIL {
                let mut value = 0.0;
                let mut path = self.nodes[node as usize].lpath;
                let first = path;
                while path != NIL {
                    let record = &self.paths[path as usize];
                    let term = record.cost + self.nodes[record.lnode as usize].alpha;
                    value = if path == first {
                        term
                    } else {
                        logsumexp(value, term)
                    };
                    path = record.lnext;
                }
                self.nodes[node as usize].alpha = value;
                node = self.nodes[node as usize].bnext;
            }
        }
    }

    fn backward(&mut self) {
        for position in (0..=self.length).rev() {
            let mut node = self.end_list[position];
            while node != NIL {
                let mut value = 0.0;
                let mut path = self.nodes[node as usize].rpath;
                let first = path;
                while path != NIL {
                    let record = &self.paths[path as usize];
                    let term = record.cost + self.nodes[record.rnode as usize].beta;
                    value = if path == first {
                        term
                    } else {
                        logsumexp(value, term)
                    };
                    path = record.rnext;
                }
                self.nodes[node as usize].beta = value;
                node = self.nodes[node as usize].enext;
            }
        }
    }

    pub(super) fn gradient(&mut self, pool: &[i32], alpha: &[f64], expected: &mut [f64]) -> f64 {
        self.viterbi(pool, alpha);
        self.forward();
        self.backward();

        let mut z = self.nodes[self.begin_list[self.length] as usize].alpha;

        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NIL {
                let mut path = self.nodes[node as usize].lpath;
                while path != NIL {
                    let record = &self.paths[path as usize];
                    if !self.is_empty(record) {
                        let left = &self.nodes[record.lnode as usize];
                        let right = &self.nodes[record.rnode as usize];
                        let weight = (left.alpha + record.cost + right.beta - z).exp();
                        let mut at = record.fvector as usize;
                        while pool[at] != ID_LIST_END {
                            expected[pool[at] as usize] += weight;
                            at += 1;
                        }
                        if right.kind != EOS {
                            let mut at = right.fvector as usize;
                            while pool[at] != ID_LIST_END {
                                expected[pool[at] as usize] += weight;
                                at += 1;
                            }
                        }
                    }
                    path = record.lnext;
                }
                node = self.nodes[node as usize].bnext;
            }
        }

        for &path in &self.answer {
            z -= self.paths[path as usize].cost;
        }
        z
    }

    pub(super) fn eval(
        &self,
        features: &Features,
        eval_size: usize,
        unk_eval_size: usize,
        correct: &mut usize,
        precision: &mut usize,
        recall: &mut usize,
    ) -> usize {
        let mut zero_one = 0;
        let start = &self.nodes[self.end_list[0] as usize];
        let (mut result, mut answer) = (start.next, start.anext);
        if result == NIL || answer == NIL {
            return 1;
        }
        let (mut result_end, mut answer_end) = (0usize, 0usize);
        let node = |index: u32| &self.nodes[index as usize];

        while node(answer).anext != NIL && node(result).next != NIL {
            match result_end.cmp(&answer_end) {
                Ordering::Equal => {
                    let (a, r) = (node(answer), node(result));
                    if same_node(
                        self.surface(answer),
                        features.get(a.feature),
                        self.surface(result),
                        features.get(r.feature),
                        r.kind == UNKNOWN,
                        eval_size,
                        unk_eval_size,
                    ) {
                        *correct += 1;
                    } else {
                        zero_one = 1;
                    }
                    *precision += 1;
                    *recall += 1;
                    result = r.next;
                    answer = a.anext;
                    result_end += node(result).rlength as usize;
                    answer_end += node(answer).rlength as usize;
                }
                Ordering::Less => {
                    result = node(result).next;
                    result_end += node(result).rlength as usize;
                    zero_one = 1;
                    *recall += 1;
                }
                Ordering::Greater => {
                    answer = node(answer).anext;
                    answer_end += node(answer).rlength as usize;
                    zero_one = 1;
                    *precision += 1;
                }
            }
        }

        while node(answer).anext != NIL {
            *precision += 1;
            answer = node(answer).anext;
        }
        while node(result).next != NIL {
            *recall += 1;
            result = node(result).next;
        }
        zero_one
    }
}

fn logsumexp(x: f64, y: f64) -> f64 {
    const MINUS_LOG_EPSILON: f64 = 50.0;
    let min = if y < x { y } else { x };
    let max = if x < y { y } else { x };
    if max > min + MINUS_LOG_EPSILON {
        max
    } else {
        max + ((min - max).exp() + 1.0).ln()
    }
}
