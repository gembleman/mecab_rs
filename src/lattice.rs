use std::ops::Range;

use crate::char_property::{CharInfo, SentenceChars};
use crate::constraint::{Constraint, ConstraintSet};
use crate::dictionary::{Dictionary, UserIndex};
use crate::error::Error;
use crate::ids::{ElementId, NodeId};
use crate::matrix::{DenseMatrix, HotView, Matrix};
use crate::nbest::{Agenda, LinkSpan, PathLink};
use crate::packed::{PackedDictionary, PackedView};
use crate::system_dictionary::PrefixHit;
use crate::system_dictionary::{SystemDictionary, TokenRecord};
use crate::utf8::sequence_len;

mod best_lefts;
mod lookup;
mod nbest;

use best_lefts::best_lefts;
use nbest::Marginals;

fn newest_first<I: DoubleEndedIterator>(candidates: I) -> std::iter::Rev<I> {
    candidates.rev()
}

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

#[inline(always)]
fn prefetch<T>(address: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(address.cast());
    };
    #[cfg(not(target_arch = "x86_64"))]
    let _ = address;
}

#[inline(always)]
fn best_left<const SELECT: bool>(
    lefts: &[LeftCandidate],
    mut connection_cost: impl FnMut(u16) -> i16,
) -> (i32, NodeId) {
    let mut best_cost = i32::MAX;
    let mut best = lefts[0].id;
    for left in lefts {
        let cost = left.cost + i32::from(connection_cost(left.right_id));
        if SELECT {
            (best_cost, best) = std::hint::select_unpredictable(
                cost <= best_cost,
                (cost, left.id),
                (best_cost, best),
            );
        } else if cost <= best_cost {
            best_cost = cost;
            best = left.id;
        }
    }
    (best_cost, best)
}

const CONNECT_GROUP_SIZE: usize = 4;

pub(crate) const END_SLOTS_PAST_TEXT: usize = 2;

const EOS_END_PAST_LAST_FILLED: usize = 1;

pub(crate) const MAX_TEXT_LEN: usize =
    u32::MAX as usize - END_SLOTS_PAST_TEXT - EOS_END_PAST_LAST_FILLED;

const RETAINED_MIN_ENTRIES: usize = 4096;

const RETAINED_ENTRIES_PER_BYTE: usize = 256;

fn retained_capacity(sentence_len: usize) -> usize {
    RETAINED_MIN_ENTRIES.max(sentence_len.saturating_mul(RETAINED_ENTRIES_PER_BYTE))
}

fn shrink_over<T>(buffer: &mut Vec<T>, keep: usize) {
    if buffer.capacity() > keep {
        buffer.shrink_to(keep);
    }
}

pub(crate) const MAX_PATH_LINKS: usize = u32::MAX as usize;

fn links_fit(existing: usize, added: usize) -> bool {
    existing
        .checked_add(added)
        .is_some_and(|total| total <= MAX_PATH_LINKS)
}

#[cfg(feature = "pair-stats")]
fn record_pairs(nodes: &[BestNode], lefts: &[LeftCandidate]) {
    for node in nodes {
        for left in lefts {
            crate::pair_stats::record(node.candidate.left_id, left.right_id);
        }
    }
}

#[inline(always)]
fn prefetch_columns(
    nodes: &[BestNode],
    lefts: &[LeftCandidate],
    hot: &HotView<'_>,
    reaches: impl Fn(&[i16]) -> bool,
) {
    for node in nodes {
        let cells = hot.right_column(node.candidate.left_id).cells();
        if reaches(cells) {
            for left in lefts {
                prefetch(cells.as_ptr().wrapping_add(usize::from(left.right_id)));
            }
        }
    }
}

#[inline(always)]
fn store_best(
    best_nodes: &mut [BestNode],
    ends: &mut [EndSlot],
    position: usize,
    index: usize,
    cost_without_word: i32,
    best: NodeId,
) {
    let node = &mut best_nodes[index];
    let cost = cost_without_word + i32::from(node.candidate.word_cost);
    node.prev = Some(best);
    node.cost = cost;
    ends[node.candidate.end as usize - position - 1].push(LeftCandidate {
        right_id: node.candidate.right_id,
        cost,
        id: NodeId::from_index(index),
    });
}

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
            self.build_best(dictionary, text.as_bytes());
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
        let keep_slots = RETAINED_MIN_ENTRIES.max(slots);
        if self.ends.len() > keep_slots {
            self.ends.truncate(keep_slots);
            self.ends.shrink_to(keep_slots);
        }
    }

    fn build<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        sentence: &[u8],
    ) -> Result<NodeId, Error> {
        self.build_with::<NBEST>(
            dictionary,
            &dictionary.system,
            &dictionary.unknown,
            sentence,
        )
    }

    #[inline(always)]
    fn build_with<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
        unknown: &PackedDictionary,
        sentence: &[u8],
    ) -> Result<NodeId, Error> {
        let sentence_len = sentence.len();
        let view = system.view();
        self.sentence_chars
            .fill(&dictionary.chars, sentence, |code| view.trie_code_of(code));
        self.push_bos::<false>();
        for position in 0..sentence_len {
            if self.ends[position].lefts.is_empty() {
                continue;
            }
            let begun =
                self.lookup::<false>(dictionary, system, &view, unknown, sentence, position)?;
            self.connect::<NBEST>(dictionary, position, begun)?;
        }
        let last_filled_end = self.last_filled_end_slot(sentence_len);
        let eos_index = self.push_eos::<false>(sentence_len, 0);
        if let Some(position) = last_filled_end {
            self.connect::<NBEST>(dictionary, position, eos_index..eos_index + 1)?;
        }
        Ok(NodeId::from_index(eos_index))
    }

    fn build_best(&mut self, dictionary: &Dictionary, sentence: &[u8]) {
        self.build_best_with(
            dictionary,
            &dictionary.system,
            &dictionary.unknown,
            sentence,
        );
    }

    #[inline(always)]
    fn build_best_with(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
        unknown: &PackedDictionary,
        sentence: &[u8],
    ) {
        let sentence_len = sentence.len();
        let view = system.view();
        self.sentence_chars
            .fill(&dictionary.chars, sentence, |code| view.trie_code_of(code));
        self.push_bos::<true>();
        let mut position = 0;
        while position < sentence_len {
            let lead = sentence[position];
            if !self.ends[position].lefts.is_empty() {
                let first = self.best_nodes.len();
                let found =
                    self.lookup::<true>(dictionary, system, &view, unknown, sentence, position);
                debug_assert!(found.is_ok());
                self.connect_best(dictionary, position, first);
            }
            position += sequence_len(lead);
        }
        let last_filled_end = self.last_filled_end_slot(sentence_len).unwrap_or(0);
        let eos = self.push_eos::<true>(sentence_len, last_filled_end);
        self.connect_best(dictionary, last_filled_end, eos);
        self.collect_best_nodes();
    }

    #[inline(always)]
    fn last_filled_end_slot(&self, sentence_len: usize) -> Option<usize> {
        (0..=sentence_len)
            .rev()
            .find(|&at| !self.ends[at].lefts.is_empty())
    }

    #[inline(always)]
    fn push_bos<const BEST: bool>(&mut self) {
        let bos = if BEST {
            self.best_nodes.push(BestNode {
                candidate: Candidate::bos(),
                prev: None,
                cost: 0,
            });
            NodeId::from_index(self.best_nodes.len() - 1)
        } else {
            self.push_node(RawNode::boundary(NodeSource::Bos, 0))
        };
        self.ends[0].push(LeftCandidate {
            right_id: 0,
            cost: 0,
            id: bos,
        });
    }

    #[inline(always)]
    fn push_eos<const BEST: bool>(&mut self, sentence_len: usize, last_filled_end: usize) -> usize {
        debug_assert!(last_filled_end <= sentence_len && sentence_len <= MAX_TEXT_LEN);
        if BEST {
            self.best_nodes.push(BestNode {
                candidate: Candidate {
                    surface: sentence_len as u32,
                    end: last_filled_end as u32 + EOS_END_PAST_LAST_FILLED as u32,
                    source: NodeSource::Eos,
                    ..Candidate::bos()
                },
                prev: None,
                cost: 0,
            });
            self.best_nodes.len() - 1
        } else {
            self.push_node(RawNode::boundary(NodeSource::Eos, sentence_len))
                .index()
        }
    }

    fn connect_best(&mut self, dictionary: &Dictionary, position: usize, first: usize) {
        let Self {
            best_nodes, ends, ..
        } = self;
        let (head, later_ends) = ends.split_at_mut(position + 1);
        let site = BestSite {
            lefts: &head[position].lefts,
            widest: head[position].widest,
            position,
            first,
        };
        match &dictionary.matrix {
            Matrix::PackedDense(dense) => {
                Self::connect_best_dense(best_nodes, later_ends, &site, dense);
            }
            Matrix::PackedHot(hot) => {
                Self::connect_best_hot(best_nodes, later_ends, &site, &hot.view());
            }
        }
    }

    #[inline(always)]
    fn connect_best_dense(
        best_nodes: &mut [BestNode],
        later_ends: &mut [EndSlot],
        site: &BestSite<'_>,
        dense: &DenseMatrix,
    ) {
        for index in (site.first..best_nodes.len()).rev() {
            let column = dense.right_column(best_nodes[index].candidate.left_id);
            let (cost, best) = best_left::<false>(site.lefts, |id| {
                debug_assert!(usize::from(id) < column.len());
                unsafe { *column.get_unchecked(usize::from(id)) }
            });
            store_best(best_nodes, later_ends, site.position, index, cost, best);
        }
    }

    #[inline(always)]
    fn connect_best_hot(
        best_nodes: &mut [BestNode],
        later_ends: &mut [EndSlot],
        site: &BestSite<'_>,
        hot: &HotView<'_>,
    ) {
        let BestSite {
            lefts,
            widest,
            position,
            first,
        } = *site;
        #[cfg(feature = "pair-stats")]
        record_pairs(&best_nodes[first..], lefts);
        let reaches = |cells: &[i16]| cells.len() > usize::from(widest);
        prefetch_columns(&best_nodes[first..], lefts, hot, reaches);
        let mut next = best_nodes.len();
        while next > first {
            if next - first >= CONNECT_GROUP_SIZE
                && Self::connect_group(best_nodes, later_ends, site, hot, next, reaches)
            {
                next -= CONNECT_GROUP_SIZE;
                continue;
            }
            next -= 1;
            let column = hot.right_column(best_nodes[next].candidate.left_id);
            let cells = column.cells();
            let (cost, best) = if reaches(cells) {
                let cost = |id: u16| unsafe { *cells.get_unchecked(usize::from(id)) };
                best_left::<true>(lefts, cost)
            } else {
                best_left::<true>(lefts, column.into_cost_fn())
            };
            store_best(best_nodes, later_ends, position, next, cost, best);
        }
    }

    #[inline(always)]
    fn connect_group(
        best_nodes: &mut [BestNode],
        later_ends: &mut [EndSlot],
        site: &BestSite<'_>,
        hot: &HotView<'_>,
        next: usize,
        reaches: impl Fn(&[i16]) -> bool,
    ) -> bool {
        let indices: [usize; CONNECT_GROUP_SIZE] = std::array::from_fn(|lane| next - 1 - lane);
        let columns = indices.map(|index| {
            hot.right_column(best_nodes[index].candidate.left_id)
                .cells()
        });
        if !columns.iter().all(|cells| reaches(cells)) {
            return false;
        }
        let found = best_lefts(site.lefts, columns);
        for (index, (cost, best)) in indices.into_iter().zip(found) {
            store_best(best_nodes, later_ends, site.position, index, cost, best);
        }
        true
    }

    fn collect_best_nodes(&mut self) {
        let eos = NodeId::from_index(self.best_nodes.len() - 1);
        self.path_cost = i64::from(self.best_nodes[eos].cost);
        let path_len = self.path_len(eos);
        self.nodes
            .resize(path_len, RawNode::boundary(NodeSource::Bos, 0));
        self.path.extend((0..path_len).map(NodeId::from_index));
        let mut slot = path_len;
        let mut cursor = Some(eos);
        while let Some(id) = cursor {
            slot -= 1;
            let best = self.best_nodes[id];
            self.nodes[slot] = self.materialize_node(best, slot);
            cursor = best.prev;
        }
    }

    #[inline(always)]
    fn path_len(&self, eos: NodeId) -> usize {
        let mut len = 0;
        let mut cursor = Some(eos);
        while let Some(id) = cursor {
            len += 1;
            cursor = self.best_nodes[id].prev;
        }
        len
    }

    #[inline(always)]
    fn materialize_node(&self, best: BestNode, slot: usize) -> RawNode {
        let candidate = best.candidate;
        let mut node = match candidate.source {
            NodeSource::Bos | NodeSource::Eos => {
                RawNode::boundary(candidate.source, candidate.surface as usize)
            }
            _ => {
                let begin = self.best_nodes[best.prev.expect("a word has a left node")]
                    .candidate
                    .end;
                candidate.raw(begin as usize)
            }
        };
        node.cost = best.cost;
        node.prev = slot.checked_sub(1).map(NodeId::from_index);
        node
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

    fn connect<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        position: usize,
        begun: Range<usize>,
    ) -> Result<(), Error> {
        if NBEST {
            let added = begun.len().saturating_mul(self.ends[position].lefts.len());
            if !links_fit(self.links.len(), added) {
                return Err(Error::TextTooLong);
            }
        }
        let mut lefts = std::mem::take(&mut self.ends[position]);
        self.connect_rights::<NBEST>(dictionary, position, begun, &lefts.lefts, lefts.widest);
        let added = &mut self.ends[position];
        if !added.lefts.is_empty() {
            lefts.absorb(added);
        }
        std::mem::swap(added, &mut lefts);
        Ok(())
    }

    fn connect_rights<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        position: usize,
        begun: Range<usize>,
        lefts: &[LeftCandidate],
        widest: u16,
    ) {
        match &dictionary.matrix {
            Matrix::PackedDense(dense) => {
                for right_index in newest_first(begun) {
                    let right_id = NodeId::from_index(right_index);
                    let column = dense.right_column(self.nodes[right_id].left_id);
                    self.connect_right::<NBEST, false>(
                        position,
                        right_id,
                        lefts,
                        |left_right_id| {
                            debug_assert!(usize::from(left_right_id) < column.len());
                            unsafe { *column.get_unchecked(usize::from(left_right_id)) }
                        },
                    );
                }
            }
            Matrix::PackedHot(hot) => {
                let hot = hot.view();
                for right_index in newest_first(begun) {
                    let right_id = NodeId::from_index(right_index);
                    let column = hot.right_column(self.nodes[right_id].left_id);
                    let cells = column.cells();
                    if cells.len() > usize::from(widest) {
                        let cost = |left_right_id: u16| unsafe {
                            *cells.get_unchecked(usize::from(left_right_id))
                        };
                        self.connect_right::<NBEST, true>(position, right_id, lefts, cost);
                    } else {
                        self.connect_right::<NBEST, true>(
                            position,
                            right_id,
                            lefts,
                            column.into_cost_fn(),
                        );
                    }
                }
            }
        }
    }

    fn connect_right<const NBEST: bool, const SELECT: bool>(
        &mut self,
        position: usize,
        right_id: NodeId,
        lefts: &[LeftCandidate],
        mut connection_cost: impl FnMut(u16) -> i16,
    ) {
        let right_node = self.nodes[right_id];
        let word_cost = i32::from(right_node.word_cost);
        let links_start = self.links.len();
        let (best_cost, best) = if NBEST {
            let mut best_cost = i32::MAX;
            let mut best = lefts[0].id;
            for left in newest_first(lefts.iter()) {
                let link_cost = i32::from(connection_cost(left.right_id)) + word_cost;
                let cost = left.cost + link_cost;
                if cost < best_cost {
                    best_cost = cost;
                    best = left.id;
                }
                self.links.push(PathLink {
                    left_node: left.id,
                    cost: link_cost,
                });
            }
            (best_cost, best)
        } else {
            let (best_cost, best) = best_left::<SELECT>(lefts, connection_cost);
            (best_cost + word_cost, best)
        };
        let target = right_node.length_with_space as usize + position;
        let node = &mut self.nodes[right_id];
        node.prev = Some(best);
        node.cost = best_cost;
        node.links = LinkSpan::new(links_start, self.links.len());
        let ending = LeftCandidate {
            right_id: right_node.right_id,
            cost: best_cost,
            id: right_id,
        };
        self.ends[target].push(ending);
    }
}
