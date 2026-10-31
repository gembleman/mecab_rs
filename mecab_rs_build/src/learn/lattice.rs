use std::cmp::Ordering;
use std::path::Path as FilePath;

use super::features::{FeatureId, FeatureIndex, Features, PoolOffset, ids_from, widen};
use super::surface_and_feature;
use super::tokenizer::{BOS, EOS, Found, NORMAL, Tokenizer, UNKNOWN};
use crate::build::text::{Lines, cstr};
use crate::tool_error::{Position, learn_error};
use mecab_rs::Error;

const TEXT_PADDING: usize = 8;
const WORST_COST: f64 = -1e37;
const TOO_MANY_NODES: &str = "a sentence has too many nodes";
const TOO_MANY_PATHS: &str = "a sentence has too many paths";
const SENTENCE_TOO_LONG: &str = "a sentence is longer than 4294967295 bytes";
const OFFSET_WITHIN_CHECKED_SENTENCE: &str = "offsets stay within a sentence checked by read";

#[derive(Clone, Copy, PartialEq, Eq)]
struct NodeId(u32);

impl NodeId {
    const NIL: Self = Self(u32::MAX);

    fn new(count: usize) -> Option<Self> {
        u32::try_from(count).ok().map(Self)
    }

    fn index(self) -> usize {
        widen(self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PathId(u32);

impl PathId {
    const NIL: Self = Self(u32::MAX);

    fn new(count: usize) -> Option<Self> {
        u32::try_from(count).ok().map(Self)
    }

    fn index(self) -> usize {
        widen(self.0)
    }
}

fn offset(value: usize) -> u32 {
    u32::try_from(value).expect(OFFSET_WITHIN_CHECKED_SENTENCE)
}

struct Word {
    surface: Vec<u8>,
    feature: Vec<u8>,
}

#[derive(Clone)]
struct Node {
    position: u32,
    length: u32,
    rlength: u32,
    feature: FeatureId,
    fvector: PoolOffset,
    bnext: NodeId,
    enext: NodeId,
    lpath: PathId,
    rpath: PathId,
    prev: NodeId,
    next: NodeId,
    anext: NodeId,
    kind: u8,
    char_type: u8,
    alpha: f64,
    beta: f64,
    wcost: f64,
    cost: f64,
}

impl Node {
    fn new(kind: u8, feature: FeatureId) -> Self {
        Self {
            position: 0,
            length: 0,
            rlength: 0,
            feature,
            fvector: PoolOffset::NONE,
            bnext: NodeId::NIL,
            enext: NodeId::NIL,
            lpath: PathId::NIL,
            rpath: PathId::NIL,
            prev: NodeId::NIL,
            next: NodeId::NIL,
            anext: NodeId::NIL,
            kind,
            char_type: 0,
            alpha: 0.0,
            beta: 0.0,
            wcost: 0.0,
            cost: 0.0,
        }
    }

    fn start(&self) -> usize {
        widen(self.position)
    }

    fn end(&self) -> usize {
        self.start() + widen(self.length)
    }
}

#[derive(Clone)]
struct Path {
    rnode: NodeId,
    lnode: NodeId,
    rnext: PathId,
    lnext: PathId,
    fvector: PoolOffset,
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

#[derive(Clone, Copy)]
pub(super) struct EvalSizes {
    pub(super) eval_size: usize,
    pub(super) unk_eval_size: usize,
}

#[derive(Default)]
pub(super) struct Tally {
    pub(super) correct: usize,
    pub(super) precision: usize,
    pub(super) recall: usize,
}

struct Alignment {
    result: NodeId,
    answer: NodeId,
    result_end: usize,
    answer_end: usize,
}

pub(super) struct ReadSentence {
    pub(super) sentence: Sentence,
    pub(super) virtual_features: Vec<Vec<u8>>,
}

struct AnswerWords {
    words: Vec<Word>,
    text: Vec<u8>,
}

fn read_words(lines: &mut Lines, path: &FilePath) -> Result<Option<AnswerWords>, Error> {
    let mut words: Vec<Word> = Vec::new();
    let mut text = Vec::new();
    loop {
        let Some((number, line)) = lines.next_line()? else {
            if words.is_empty() {
                return Ok(None);
            }
            return Err(learn_error(path, "missing EOS at the end of the corpus"));
        };
        let line = cstr(line, 0);
        if line == b"EOS" || line.is_empty() {
            break;
        }
        let at = Position::new(path, number);
        let Some((surface, feature)) = surface_and_feature(line) else {
            return Err(at.learn(format!("format error: {}", String::from_utf8_lossy(line))));
        };
        if surface.len() > usize::from(u16::MAX) {
            return Err(at.learn("a surface is longer than 65535 bytes"));
        }
        text.extend_from_slice(surface);
        words.push(Word {
            surface: surface.to_vec(),
            feature: feature.to_vec(),
        });
    }
    if text.is_empty() {
        return Err(learn_error(path, "empty sentence"));
    }
    if u32::try_from(text.len()).is_err() {
        return Err(learn_error(path, SENTENCE_TOO_LONG));
    }
    Ok(Some(AnswerWords { words, text }))
}

pub(super) struct Sentence {
    text: Vec<u8>,
    length: usize,
    nodes: Vec<Node>,
    paths: Vec<Path>,
    begin_list: Vec<NodeId>,
    end_list: Vec<NodeId>,
    answer: Vec<PathId>,
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
    fn node(&mut self, node: Node, source: &FilePath) -> Result<NodeId, Error> {
        let index =
            NodeId::new(self.nodes.len()).ok_or_else(|| learn_error(source, TOO_MANY_NODES))?;
        self.nodes.push(node);
        Ok(index)
    }

    fn surface(&self, node: NodeId) -> &[u8] {
        let node = &self.nodes[node.index()];
        &self.text[node.start()..node.end()]
    }

    fn lookup(
        &mut self,
        reader: &mut Reader,
        position: usize,
        source: &FilePath,
    ) -> Result<NodeId, Error> {
        if self.begin_list[position] != NodeId::NIL {
            return Ok(self.begin_list[position]);
        }
        reader
            .tokenizer
            .lookup(&self.text, position, self.length, &mut reader.found);
        let mut head = NodeId::NIL;
        for index in 0..reader.found.len() {
            let found = reader.found[index];
            let feature = reader.tokenizer.feature(found.feature);
            let mut node = Node::new(found.kind, reader.features.intern(feature)?);
            node.position = offset(found.position);
            node.length = offset(found.length);
            node.rlength = offset(found.rlength);
            node.char_type = found.char_type;
            node.bnext = head;
            head = self.node(node, source)?;
        }
        self.begin_list[position] = head;
        Ok(head)
    }

    fn connect(
        &mut self,
        reader: &mut Reader,
        position: usize,
        first: NodeId,
        source: &FilePath,
    ) -> Result<(), Error> {
        let mut right = first;
        while right != NodeId::NIL {
            let mut left = self.end_list[position];
            while left != NodeId::NIL {
                let path = PathId::new(self.paths.len())
                    .ok_or_else(|| learn_error(source, TOO_MANY_PATHS))?;
                self.paths.push(Path {
                    rnode: right,
                    lnode: left,
                    rnext: self.nodes[left.index()].rpath,
                    lnext: self.nodes[right.index()].lpath,
                    fvector: PoolOffset::NONE,
                    cost: 0.0,
                });
                self.nodes[right.index()].lpath = path;
                self.nodes[left.index()].rpath = path;
                self.build_feature(reader, path)?;
                left = self.nodes[left.index()].enext;
            }
            let end = widen(self.nodes[right.index()].rlength) + position;
            self.nodes[right.index()].enext = self.end_list[end];
            self.end_list[end] = right;
            right = self.nodes[right.index()].bnext;
        }
        Ok(())
    }

    fn build_feature(&mut self, reader: &mut Reader, path: PathId) -> Result<(), Error> {
        let (left, right) = {
            let path = &self.paths[path.index()];
            (path.lnode, path.rnode)
        };
        self.nodes[right.index()].wcost = 0.0;
        self.paths[path.index()].cost = 0.0;
        let left_feature = self.nodes[left.index()].feature;
        let right_feature = self.nodes[right.index()].feature;
        reader.features.rewrite(left_feature)?;
        reader.features.rewrite(right_feature)?;
        let right_node = &self.nodes[right.index()];
        let surface: &[u8] = if right_node.kind == NORMAL {
            &self.text[right_node.start()..right_node.end()]
        } else {
            b""
        };
        let unigram = reader.index.unigram(
            &reader.features.rewritten(right_feature).unigram,
            right_node.char_type,
            surface,
        )?;
        self.nodes[right.index()].fvector = unigram;
        let bigram = reader.index.bigram(
            &reader.features.rewritten(left_feature).right,
            &reader.features.rewritten(right_feature).left,
        )?;
        self.paths[path.index()].fvector = bigram;
        Ok(())
    }

    fn build_lattice(&mut self, reader: &mut Reader, source: &FilePath) -> Result<(), Error> {
        for position in 0..=self.length {
            if self.end_list[position] == NodeId::NIL {
                continue;
            }
            let first = self.lookup(reader, position, source)?;
            self.connect(reader, position, first, source)?;
        }
        if self.end_list[self.length] == NodeId::NIL {
            self.begin_list[self.length] = self.lookup(reader, self.length, source)?;
            for position in (0..=self.length).rev() {
                if self.end_list[position] != NodeId::NIL {
                    let first = self.begin_list[self.length];
                    self.connect(reader, position, first, source)?;
                    break;
                }
            }
        }
        Ok(())
    }

    fn with_text(mut text: Vec<u8>) -> Self {
        let length = text.len();
        text.extend_from_slice(&[0; TEXT_PADDING]);
        Self {
            text,
            length,
            nodes: Vec::new(),
            paths: Vec::new(),
            begin_list: vec![NodeId::NIL; length + TEXT_PADDING],
            end_list: vec![NodeId::NIL; length + TEXT_PADDING],
            answer: Vec::new(),
        }
    }

    fn add_boundary_nodes(&mut self, reader: &mut Reader, source: &FilePath) -> Result<(), Error> {
        let bos_feature = reader.features.intern(&reader.tokenizer.bos_feature)?;
        let bos = self.node(Node::new(BOS, bos_feature), source)?;
        let eos = self.node(Node::new(EOS, bos_feature), source)?;
        self.end_list[0] = bos;
        self.begin_list[self.length] = eos;
        Ok(())
    }

    fn has_matching_node(
        &mut self,
        reader: &mut Reader,
        word: &Word,
        position: usize,
        source: &FilePath,
    ) -> Result<bool, Error> {
        let mut node = self.lookup(reader, position, source)?;
        while node != NodeId::NIL {
            if self.matches(reader, word, node) {
                return Ok(true);
            }
            node = self.nodes[node.index()].bnext;
        }
        Ok(false)
    }

    fn add_virtual_node(
        &mut self,
        reader: &mut Reader,
        word: &Word,
        position: usize,
        source: &FilePath,
    ) -> Result<(), Error> {
        let mut node = Node::new(NORMAL, reader.features.intern(&word.feature)?);
        node.position = offset(position);
        node.length = offset(word.surface.len());
        node.rlength = offset(word.surface.len());
        node.bnext = self.begin_list[position];
        let node = self.node(node, source)?;
        self.begin_list[position] = node;
        Ok(())
    }

    fn add_virtual_nodes(
        &mut self,
        reader: &mut Reader,
        words: &[Word],
        source: &FilePath,
    ) -> Result<Vec<Vec<u8>>, Error> {
        let mut virtual_features = Vec::new();
        let mut position = 0;
        for word in words {
            if !self.has_matching_node(reader, word, position, source)? {
                self.add_virtual_node(reader, word, position, source)?;
                virtual_features.push(word.feature.clone());
            }
            position += word.surface.len();
        }
        Ok(virtual_features)
    }

    fn find_answer_node(&self, reader: &Reader, word: Option<&Word>, position: usize) -> NodeId {
        let mut right = NodeId::NIL;
        let mut node = self.begin_list[position];
        while node != NodeId::NIL {
            if word.is_none_or(|word| self.matches(reader, word, node)) {
                right = node;
            }
            node = self.nodes[node.index()].bnext;
        }
        right
    }

    fn find_left_path(&self, right: NodeId, left: NodeId) -> PathId {
        let mut candidate = self.nodes[right.index()].lpath;
        while candidate != PathId::NIL {
            if self.paths[candidate.index()].lnode == left {
                return candidate;
            }
            candidate = self.paths[candidate.index()].lnext;
        }
        PathId::NIL
    }

    fn count_observed(
        &self,
        reader: &Reader,
        left_path: PathId,
        right: NodeId,
        observed: &mut Vec<f64>,
    ) {
        let pool = reader.index.pool();
        let mut count = |start: PoolOffset| {
            for id in ids_from(pool, start) {
                if id >= observed.len() {
                    observed.resize(id + 1, 0.0);
                }
                observed[id] += 1.0;
            }
        };
        count(self.paths[left_path.index()].fvector);
        if self.nodes[right.index()].kind != EOS {
            count(self.nodes[right.index()].fvector);
        }
    }

    fn link_answer(
        &mut self,
        reader: &Reader,
        words: &[Word],
        observed: &mut Vec<f64>,
        source: &FilePath,
    ) -> Result<(), Error> {
        let mut prev = self.end_list[0];
        self.nodes[prev.index()].anext = NodeId::NIL;
        let mut position = 0;
        for index in 0..=words.len() {
            let word = words.get(index);
            let right = self.find_answer_node(reader, word, position);
            if right == NodeId::NIL {
                return Err(learn_error(source, "no node for a word of the answer"));
            }
            let left_path = self.find_left_path(right, prev);
            if left_path == PathId::NIL {
                return Err(learn_error(source, "lpath is NULL"));
            }
            self.count_observed(reader, left_path, right, observed);
            self.answer.push(left_path);
            self.nodes[prev.index()].anext = right;
            prev = right;
            let Some(word) = word else {
                break;
            };
            position += word.surface.len();
        }
        let eos_head = self.begin_list[self.length];
        self.nodes[prev.index()].anext = eos_head;
        self.nodes[eos_head.index()].anext = NodeId::NIL;
        Ok(())
    }

    pub(super) fn read(
        reader: &mut Reader,
        lines: &mut Lines,
        observed: &mut Vec<f64>,
    ) -> Result<Option<ReadSentence>, Error> {
        let path = lines.path().to_path_buf();
        let Some(AnswerWords { words, text }) = read_words(lines, &path)? else {
            return Ok(None);
        };
        let mut sentence = Self::with_text(text);
        sentence.add_boundary_nodes(reader, &path)?;
        let virtual_features = sentence.add_virtual_nodes(reader, &words, &path)?;
        sentence.build_lattice(reader, &path)?;
        sentence.link_answer(reader, &words, observed, &path)?;
        Ok(Some(ReadSentence {
            sentence,
            virtual_features,
        }))
    }

    fn matches(&self, reader: &Reader, word: &Word, node: NodeId) -> bool {
        let n = &self.nodes[node.index()];
        same_node(
            &word.surface,
            &word.feature,
            self.surface(node),
            reader.features.get(n.feature),
            n.kind == UNKNOWN,
            reader.eval_size,
            reader.unk_eval_size,
        )
    }

    fn is_empty(&self, path: &Path) -> bool {
        let right = &self.nodes[path.rnode.index()];
        let left = &self.nodes[path.lnode.index()];
        (right.rpath == PathId::NIL && right.kind != EOS)
            || (left.lpath == PathId::NIL && left.kind != BOS)
    }

    fn viterbi(&mut self, pool: &[i32], alpha: &[f64]) {
        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NodeId::NIL {
                let mut wcost = 0.0;
                if self.nodes[node.index()].kind != EOS {
                    for id in ids_from(pool, self.nodes[node.index()].fvector) {
                        wcost += alpha[id];
                    }
                }
                self.nodes[node.index()].wcost = wcost;

                let mut best_cost = WORST_COST;
                let mut best = NodeId::NIL;
                let mut path = self.nodes[node.index()].lpath;
                while path != PathId::NIL {
                    if !self.is_empty(&self.paths[path.index()]) {
                        let record = &self.paths[path.index()];
                        let mut cost = self.nodes[record.rnode.index()].wcost;
                        for id in ids_from(pool, record.fvector) {
                            cost += alpha[id];
                        }
                        self.paths[path.index()].cost = cost;
                    }
                    let record = &self.paths[path.index()];
                    let cost = record.cost + self.nodes[record.lnode.index()].cost;
                    if cost > best_cost {
                        best_cost = cost;
                        best = record.lnode;
                    }
                    path = record.lnext;
                }
                let node_ref = &mut self.nodes[node.index()];
                node_ref.prev = best;
                node_ref.cost = best_cost;
                node = node_ref.bnext;
            }
        }

        let mut node = self.begin_list[self.length];
        while self.nodes[node.index()].prev != NodeId::NIL {
            let prev = self.nodes[node.index()].prev;
            self.nodes[prev.index()].next = node;
            node = prev;
        }
    }

    fn forward(&mut self) {
        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NodeId::NIL {
                let mut value = 0.0;
                let mut path = self.nodes[node.index()].lpath;
                let first = path;
                while path != PathId::NIL {
                    let record = &self.paths[path.index()];
                    let term = record.cost + self.nodes[record.lnode.index()].alpha;
                    value = if path == first {
                        term
                    } else {
                        logsumexp(value, term)
                    };
                    path = record.lnext;
                }
                self.nodes[node.index()].alpha = value;
                node = self.nodes[node.index()].bnext;
            }
        }
    }

    fn backward(&mut self) {
        for position in (0..=self.length).rev() {
            let mut node = self.end_list[position];
            while node != NodeId::NIL {
                let mut value = 0.0;
                let mut path = self.nodes[node.index()].rpath;
                let first = path;
                while path != PathId::NIL {
                    let record = &self.paths[path.index()];
                    let term = record.cost + self.nodes[record.rnode.index()].beta;
                    value = if path == first {
                        term
                    } else {
                        logsumexp(value, term)
                    };
                    path = record.rnext;
                }
                self.nodes[node.index()].beta = value;
                node = self.nodes[node.index()].enext;
            }
        }
    }

    pub(super) fn gradient(&mut self, pool: &[i32], alpha: &[f64], expected: &mut [f64]) -> f64 {
        self.viterbi(pool, alpha);
        self.forward();
        self.backward();

        let mut z = self.nodes[self.begin_list[self.length].index()].alpha;

        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NodeId::NIL {
                let mut path = self.nodes[node.index()].lpath;
                while path != PathId::NIL {
                    let record = &self.paths[path.index()];
                    if !self.is_empty(record) {
                        let left = &self.nodes[record.lnode.index()];
                        let right = &self.nodes[record.rnode.index()];
                        let weight = (left.alpha + record.cost + right.beta - z).exp();
                        for id in ids_from(pool, record.fvector) {
                            expected[id] += weight;
                        }
                        if right.kind != EOS {
                            for id in ids_from(pool, right.fvector) {
                                expected[id] += weight;
                            }
                        }
                    }
                    path = record.lnext;
                }
                node = self.nodes[node.index()].bnext;
            }
        }

        for &path in &self.answer {
            z -= self.paths[path.index()].cost;
        }
        z
    }

    fn node_at(&self, index: NodeId) -> &Node {
        &self.nodes[index.index()]
    }

    fn advance_alignment(
        &self,
        alignment: &mut Alignment,
        features: &Features,
        sizes: EvalSizes,
        tally: &mut Tally,
    ) -> bool {
        match alignment.result_end.cmp(&alignment.answer_end) {
            Ordering::Equal => {
                let (a, r) = (
                    self.node_at(alignment.answer),
                    self.node_at(alignment.result),
                );
                let agree = same_node(
                    self.surface(alignment.answer),
                    features.get(a.feature),
                    self.surface(alignment.result),
                    features.get(r.feature),
                    r.kind == UNKNOWN,
                    sizes.eval_size,
                    sizes.unk_eval_size,
                );
                tally.correct += usize::from(agree);
                tally.precision += 1;
                tally.recall += 1;
                alignment.result = r.next;
                alignment.answer = a.anext;
                alignment.result_end += widen(self.node_at(alignment.result).rlength);
                alignment.answer_end += widen(self.node_at(alignment.answer).rlength);
                !agree
            }
            Ordering::Less => {
                alignment.result = self.node_at(alignment.result).next;
                alignment.result_end += widen(self.node_at(alignment.result).rlength);
                tally.recall += 1;
                true
            }
            Ordering::Greater => {
                alignment.answer = self.node_at(alignment.answer).anext;
                alignment.answer_end += widen(self.node_at(alignment.answer).rlength);
                tally.precision += 1;
                true
            }
        }
    }

    pub(super) fn eval(&self, features: &Features, sizes: EvalSizes, tally: &mut Tally) -> usize {
        let start = self.node_at(self.end_list[0]);
        let mut alignment = Alignment {
            result: start.next,
            answer: start.anext,
            result_end: 0,
            answer_end: 0,
        };
        if alignment.result == NodeId::NIL || alignment.answer == NodeId::NIL {
            return 1;
        }
        let mut zero_one = 0;
        while self.node_at(alignment.answer).anext != NodeId::NIL
            && self.node_at(alignment.result).next != NodeId::NIL
        {
            if self.advance_alignment(&mut alignment, features, sizes, tally) {
                zero_one = 1;
            }
        }
        while self.node_at(alignment.answer).anext != NodeId::NIL {
            tally.precision += 1;
            alignment.answer = self.node_at(alignment.answer).anext;
        }
        while self.node_at(alignment.result).next != NodeId::NIL {
            tally.recall += 1;
            alignment.result = self.node_at(alignment.result).next;
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
