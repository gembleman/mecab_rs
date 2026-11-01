use crate::char_property::{CharInfo, SentenceChars};
use crate::constraint::{Constraint, ConstraintSet};
use crate::dictionary::{Dictionary, UserIndex};
use crate::error::Error;
use crate::ids::{ElementId, NodeId};
use crate::nbest::{Agenda, LinkSpan, PathLink};
use crate::packed::{PackedDictionary, PackedView};
use crate::system_dictionary::PrefixHit;
use crate::system_dictionary::{SystemDictionary, TokenRecord};

mod best_lefts;
mod build;
mod connect;
mod connect_best;
mod cost;
mod lookup;
mod nbest;

use nbest::Marginals;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeSource {
    System,
    SystemExtended,
    User(UserIndex),
    Unknown,
    UnknownFixed,
    Constrained,
    Bos,
    Eos,
}

#[derive(Clone, Copy)]
pub(crate) struct RawNode {
    pub(crate) surface_start: u32,
    pub(crate) length: u32,
    pub(crate) length_with_space: u32,
    pub(crate) left_id: u16,
    pub(crate) right_id: u16,
    pub(crate) word_cost: i16,
    pub(crate) pos_id: u16,
    pub(crate) source: NodeSource,
    pub(crate) char_type: u8,
    pub(crate) feature: u32,
    pub(crate) cost: i32,
    prev: Option<NodeId>,
    links: LinkSpan,
}

impl RawNode {
    pub(crate) fn begin(&self) -> usize {
        (self.surface_start - (self.length_with_space - self.length)) as usize
    }

    fn boundary(source: NodeSource, surface_start: usize) -> Self {
        Self {
            surface_start: surface_start as u32,
            length: 0,
            length_with_space: 0,
            left_id: 0,
            right_id: 0,
            word_cost: 0,
            pos_id: 0,
            source,
            char_type: 0,
            feature: 0,
            cost: 0,
            prev: None,
            links: LinkSpan::default(),
        }
    }
}

const _: () = assert!(size_of::<RawNode>() == 44);

#[derive(Clone, Copy)]
pub(crate) struct LeftCandidate {
    right_id: u16,
    cost: i32,
    pub(crate) id: NodeId,
}

#[derive(Clone, Copy)]
struct Candidate {
    surface: u32,
    end: u32,
    feature: u32,
    left_id: u16,
    right_id: u16,
    pos_id: u16,
    word_cost: i16,
    source: NodeSource,
    char_type: u8,
}

impl Candidate {
    #[inline(always)]
    fn from_token(
        token: &TokenRecord,
        surface: usize,
        end: usize,
        source: NodeSource,
        char_type: u8,
    ) -> Self {
        Self {
            surface: surface as u32,
            end: end as u32,
            feature: token.feature,
            left_id: token.left_id,
            right_id: token.right_id,
            pos_id: token.pos_id,
            word_cost: token.word_cost,
            source,
            char_type,
        }
    }

    fn bos() -> Self {
        Self {
            surface: 0,
            end: 0,
            feature: 0,
            left_id: 0,
            right_id: 0,
            pos_id: 0,
            word_cost: 0,
            source: NodeSource::Bos,
            char_type: 0,
        }
    }

    fn raw(self, begin: usize) -> RawNode {
        RawNode {
            surface_start: self.surface,
            length: self.end - self.surface,
            length_with_space: self.end - begin as u32,
            left_id: self.left_id,
            right_id: self.right_id,
            word_cost: self.word_cost,
            pos_id: self.pos_id,
            source: self.source,
            char_type: self.char_type,
            feature: self.feature,
            cost: 0,
            prev: None,
            links: LinkSpan::default(),
        }
    }
}

#[derive(Clone, Copy)]
struct BestNode {
    candidate: Candidate,
    prev: Option<NodeId>,
    cost: i32,
}

struct HitSite {
    begin: usize,
    surface: usize,
    partial: bool,
    char_type: u8,
    limit: usize,
    key_end: usize,
    whole: bool,
}

struct Span {
    begin: usize,
    surface: usize,
    end: usize,
}

impl Span {
    #[inline(always)]
    fn new(begin: usize, surface: usize, end: usize) -> Self {
        Self {
            begin,
            surface,
            end,
        }
    }
}

struct BestSite<'a> {
    lefts: &'a [LeftCandidate],
    widest: u16,
    position: usize,
    first: usize,
}

#[derive(Default)]
pub(crate) struct Lattice {
    nodes: Vec<RawNode>,
    best_nodes: Vec<BestNode>,
    ends: Vec<EndSlot>,
    ends_used: usize,
    sentence_chars: SentenceChars,
    hits: Vec<PrefixHit>,
    path: Vec<NodeId>,
    path_cost: i64,
    cost_overflow: bool,
    constraints: ConstraintSet,
    links: Vec<PathLink>,
    agenda: Agenda,
    marginals: Marginals,
}

#[derive(Default)]
struct EndSlot {
    lefts: Vec<LeftCandidate>,
    widest: u16,
}

impl EndSlot {
    #[inline(always)]
    fn push(&mut self, left: LeftCandidate) {
        self.widest = self.widest.max(left.right_id);
        self.lefts.push(left);
    }

    fn clear(&mut self) {
        self.lefts.clear();
        self.widest = 0;
    }

    fn absorb(&mut self, added: &mut EndSlot) {
        self.lefts.append(&mut added.lefts);
        self.widest = self.widest.max(added.widest);
        added.widest = 0;
    }
}

const CONNECT_GROUP_SIZE: usize = 4;

pub(crate) const END_SLOTS_PAST_TEXT: usize = 2;

const EOS_END_PAST_LAST_FILLED: usize = 1;

pub(crate) const MAX_TEXT_LEN: usize =
    u32::MAX as usize - END_SLOTS_PAST_TEXT - EOS_END_PAST_LAST_FILLED;

pub(crate) const RETAINED_MIN_ENTRIES: usize = 4096;

const RETAINED_ENTRIES_PER_BYTE: usize = 256;

fn retained_capacity(sentence_len: usize) -> usize {
    RETAINED_MIN_ENTRIES.max(sentence_len.saturating_mul(RETAINED_ENTRIES_PER_BYTE))
}

pub(crate) fn shrink_over<T>(buffer: &mut Vec<T>, keep: usize) {
    if buffer.capacity() > keep {
        buffer.shrink_to(keep);
    }
}

pub(crate) fn shrink_discarding<T>(buffer: &mut Vec<T>, keep: usize) {
    if buffer.capacity() > keep {
        buffer.clear();
    }
    shrink_over(buffer, keep);
}

pub(crate) const MAX_NODE_COST: i32 = 1 << 30;

pub(crate) const MAX_PATH_LINKS: usize = u32::MAX as usize;

impl Lattice {
    pub(crate) fn parse(
        &mut self,
        dictionary: &Dictionary,
        text: &str,
        constraints: &[Constraint<'_>],
    ) -> Result<(), Error> {
        self.reset(text, constraints);
        let eos = self.build::<false>(dictionary, text.as_bytes())?;
        self.collect_best(eos);
        Ok(())
    }

    pub(crate) fn parse_best(
        &mut self,
        dictionary: &Dictionary,
        text: &str,
        constraints: &[Constraint<'_>],
    ) -> Result<(), Error> {
        self.reset(text, constraints);
        if self.constraints.is_active() {
            let eos = self.build::<false>(dictionary, text.as_bytes())?;
            self.collect_best(eos);
        } else {
            self.build_best(dictionary, text.as_bytes())?;
        }
        Ok(())
    }

    fn collect_best(&mut self, eos: NodeId) {
        let mut cursor = Some(eos);
        while let Some(id) = cursor {
            self.path.push(id);
            cursor = self.nodes[id].prev;
        }
        self.path.reverse();
        self.path_cost = i64::from(self.nodes[eos].cost);
    }

    fn reset(&mut self, text: &str, constraints: &[Constraint<'_>]) {
        let sentence_len = text.len();
        debug_assert!(sentence_len <= MAX_TEXT_LEN);
        self.nodes.clear();
        self.best_nodes.clear();
        self.path.clear();
        self.links.clear();
        self.marginals.ready = false;
        self.cost_overflow = false;
        self.constraints.load(sentence_len, constraints);
        for ending in &mut self.ends[..self.ends_used] {
            ending.clear();
        }
        let slots = sentence_len + END_SLOTS_PAST_TEXT;
        self.shrink_oversized(sentence_len, slots);
        if self.ends.len() < slots {
            self.ends.resize_with(slots, EndSlot::default);
        }
        self.ends_used = slots;
    }

    fn shrink_oversized(&mut self, sentence_len: usize, slots: usize) {
        let keep = retained_capacity(sentence_len);
        shrink_over(&mut self.nodes, keep);
        shrink_over(&mut self.best_nodes, keep);
        shrink_over(&mut self.hits, keep);
        shrink_over(&mut self.path, keep);
        shrink_over(&mut self.links, keep);
        self.marginals.shrink_oversized(keep);
        self.agenda.shrink_oversized(keep);
        let keep_slots = RETAINED_MIN_ENTRIES.max(slots);
        if self.ends.len() > keep_slots {
            self.ends.truncate(keep_slots);
            self.ends.shrink_to(keep_slots);
        }
    }

    pub(crate) fn nodes(&self) -> &[RawNode] {
        &self.nodes
    }

    pub(crate) fn ending_at(&self, position: usize) -> &[LeftCandidate] {
        self.ends
            .get(position)
            .map_or(&[], |slot| slot.lefts.as_slice())
    }

    pub(crate) fn path(&self) -> &[NodeId] {
        &self.path
    }

    pub(crate) fn path_cost(&self) -> i64 {
        self.path_cost
    }

    pub(crate) fn constraints(&self) -> &ConstraintSet {
        &self.constraints
    }

    fn push_node(&mut self, node: RawNode) -> NodeId {
        self.nodes.push(node);
        NodeId::from_index(self.nodes.len() - 1)
    }

    fn node_count<const BEST: bool>(&self) -> usize {
        if BEST {
            self.best_nodes.len()
        } else {
            self.nodes.len()
        }
    }

    #[inline(always)]
    fn push_candidate<const BEST: bool>(&mut self, begin: usize, candidate: Candidate) {
        if BEST {
            self.best_nodes.push(BestNode {
                candidate,
                prev: None,
                cost: 0,
            });
        } else {
            self.push_node(candidate.raw(begin));
        }
    }
}
