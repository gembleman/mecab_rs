mod answer;
mod build;
mod eval;
mod inference;

use self::answer::{AnswerWords, read_words};
use super::features::{FeatureId, FeatureIndex, Features, PoolOffset, widen};
use super::tokenizer::{Found, Tokenizer};
use crate::build::text::Lines;
use mecab_rs::Error;

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

pub(super) struct ReadSentence {
    pub(super) sentence: Sentence,
    pub(super) virtual_features: Vec<Vec<u8>>,
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

impl Sentence {
    fn surface(&self, node: NodeId) -> &[u8] {
        let node = &self.nodes[node.index()];
        &self.text[node.start()..node.end()]
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
}
