use std::ops::Range;

use crate::char_property::{CharInfo, SentenceChars};
use crate::constraint::{Constraint, ConstraintSet};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::ids::{ElementId, NodeId};
use crate::matrix::Matrix;
use crate::nbest::{Agenda, LinkSpan, PathLink};
use crate::packed::{PackedDictionary, PackedView};
use crate::system_dictionary::PrefixHit;
use crate::system_dictionary::SystemDictionary;

fn newest_first<I: DoubleEndedIterator>(candidates: I) -> std::iter::Rev<I> {
    candidates.rev()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeSource {
    System,
    SystemExtended,
    User(u8),
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
}

struct Span {
    begin: usize,
    surface: usize,
    end: usize,
}

#[derive(Default)]
pub(crate) struct Lattice {
    nodes: Vec<RawNode>,
    best_nodes: Vec<BestNode>,
    ends: Vec<Vec<LeftCandidate>>,
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
struct Marginals {
    ready: bool,
    z: f64,
    alpha: Vec<f32>,
    beta: Vec<f32>,
    prob: Vec<f32>,
    seeded: Vec<bool>,
}

fn logsumexp(x: f64, y: f64) -> f64 {
    const MINUS_LOG_EPSILON: f64 = 50.0;
    let vmin = if y < x { y } else { x };
    let vmax = if x < y { y } else { x };
    if vmax > vmin + MINUS_LOG_EPSILON {
        vmax
    } else {
        vmax + ((vmin - vmax).exp() + 1.0).ln()
    }
}

#[inline(always)]
fn prefetch<T>(address: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(address.cast())
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

const END_SLOTS_PAST_TEXT: usize = 2;

#[inline(always)]
fn utf8_sequence_len(lead: u8) -> usize {
    match lead {
        ..0xc0 => 1,
        0xc0..0xe0 => 2,
        0xe0..0xf0 => 3,
        _ => 4,
    }
}

#[inline(always)]
fn best_lefts<const N: usize>(lefts: &[LeftCandidate], columns: [&[i16]; N]) -> [(i32, NodeId); N] {
    let mut best = [(i32::MAX, lefts[0].id); N];
    for left in lefts {
        let right_id = usize::from(left.right_id);
        for (best, cells) in best.iter_mut().zip(columns) {
            debug_assert!(right_id < cells.len());
            let cost = left.cost + i32::from(unsafe { *cells.get_unchecked(right_id) });
            *best = std::hint::select_unpredictable(cost <= best.0, (cost, left.id), *best);
        }
    }
    best
}

#[inline(always)]
fn store_best(
    best_nodes: &mut [BestNode],
    ends: &mut [Vec<LeftCandidate>],
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

    pub(crate) fn parse_marginal(
        &mut self,
        dictionary: &Dictionary,
        text: &str,
        constraints: &[Constraint<'_>],
        theta: f32,
    ) -> Result<(), Error> {
        self.reset(text, constraints);
        let eos = self.build::<true>(dictionary, text.as_bytes())?;
        self.collect_best(eos);
        self.forward_backward(theta);
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

    fn forward_backward(&mut self, theta: f32) {
        let theta = f64::from(theta);
        let count = self.nodes.len();
        let marginals = &mut self.marginals;
        marginals.alpha.clear();
        marginals.alpha.resize(count, 0.0);
        marginals.beta.clear();
        marginals.beta.resize(count, 0.0);
        marginals.prob.clear();
        marginals.prob.resize(count, 0.0);
        marginals.seeded.clear();
        marginals.seeded.resize(count, false);
        for index in 1..count {
            let mut alpha = 0.0f32;
            for (step, link_index) in self.nodes[index].links.range().rev().enumerate() {
                let PathLink { left_node, cost } = self.links[link_index];
                let value = -theta * f64::from(cost) + f64::from(marginals.alpha[left_node]);
                alpha = if step == 0 {
                    value as f32
                } else {
                    logsumexp(f64::from(alpha), value) as f32
                };
            }
            marginals.alpha[index] = alpha;
        }
        let nodes = &self.nodes;
        let links = &self.links;
        let propagate_beta = |marginals: &mut Marginals, index: usize| {
            let right_beta = marginals.beta[index];
            for link_index in nodes[index].links.range().rev() {
                let PathLink { left_node, cost } = links[link_index];
                let value = -theta * f64::from(cost) + f64::from(right_beta);
                let left = left_node.index();
                marginals.beta[left] = if marginals.seeded[left] {
                    logsumexp(f64::from(marginals.beta[left]), value) as f32
                } else {
                    marginals.seeded[left] = true;
                    value as f32
                };
            }
        };
        let eos = count - 1;
        propagate_beta(marginals, eos);
        let mut end = eos;
        while end > 0 {
            let begin = nodes[end - 1].begin();
            let start = nodes[..end].partition_point(|raw| raw.begin() < begin);
            for index in start..end {
                propagate_beta(marginals, index);
            }
            end = start;
        }
        let z = f64::from(marginals.alpha[count - 1]);
        marginals.z = z;
        for index in 1..count {
            marginals.prob[index] =
                (f64::from(marginals.alpha[index] + marginals.beta[index]) - z).exp() as f32;
        }
        marginals.ready = true;
    }

    pub(crate) fn start_nbest(
        &mut self,
        dictionary: &Dictionary,
        text: &str,
        constraints: &[Constraint<'_>],
    ) -> Result<(), Error> {
        self.reset(text, constraints);
        let eos = self.build::<true>(dictionary, text.as_bytes())?;
        self.agenda.start(eos);
        Ok(())
    }

    pub(crate) fn next_nbest_path(&mut self) -> bool {
        self.path.clear();
        let Some(mut found) = self.advance_nbest() else {
            return false;
        };
        self.path_cost = self.agenda.path_cost(found);
        loop {
            self.path.push(self.agenda.node(found));
            match self.agenda.next(found) {
                Some(next) => found = next,
                None => return true,
            }
        }
    }

    fn reset(&mut self, text: &str, constraints: &[Constraint<'_>]) {
        let sentence_len = text.len();
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
        if self.ends.len() < slots {
            self.ends.resize_with(slots, Vec::new);
        }
        self.ends_used = slots;
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
        self.sentence_chars.fill(&dictionary.chars, sentence);
        let bos = self.push_node(RawNode::boundary(NodeSource::Bos, 0));
        self.ends[0].push(LeftCandidate {
            right_id: 0,
            cost: 0,
            id: bos,
        });
        for position in 0..sentence_len {
            if self.ends[position].is_empty() {
                continue;
            }
            let begun =
                self.lookup::<false>(dictionary, system, &view, unknown, sentence, position)?;
            self.connect::<NBEST>(dictionary, position, begun);
        }
        let eos_index = self.nodes.len();
        let eos = self.push_node(RawNode::boundary(NodeSource::Eos, sentence_len));
        let last_filled_end = (0..=sentence_len)
            .rev()
            .find(|&at| !self.ends[at].is_empty());
        if let Some(position) = last_filled_end {
            self.connect::<NBEST>(dictionary, position, eos_index..eos_index + 1);
        }
        Ok(eos)
    }

    fn build_best(&mut self, dictionary: &Dictionary, sentence: &[u8]) {
        self.build_best_with(
            dictionary,
            &dictionary.system,
            &dictionary.unknown,
            sentence,
        )
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
        self.sentence_chars.fill(&dictionary.chars, sentence);
        let bos_candidate = Candidate {
            surface: 0,
            end: 0,
            feature: 0,
            left_id: 0,
            right_id: 0,
            pos_id: 0,
            word_cost: 0,
            source: NodeSource::Bos,
            char_type: 0,
        };
        self.best_nodes.push(BestNode {
            candidate: bos_candidate,
            prev: None,
            cost: 0,
        });
        self.ends[0].push(LeftCandidate {
            right_id: 0,
            cost: 0,
            id: NodeId::from_index(0),
        });
        let mut position = 0;
        while position < sentence_len {
            let lead = sentence[position];
            if !self.ends[position].is_empty() {
                let first = self.best_nodes.len();
                let found =
                    self.lookup::<true>(dictionary, system, &view, unknown, sentence, position);
                debug_assert!(found.is_ok());
                self.connect_best(dictionary, position, first);
            }
            position += utf8_sequence_len(lead);
        }
        let last_filled_end = (0..=sentence_len)
            .rev()
            .find(|&at| !self.ends[at].is_empty())
            .unwrap_or(0);
        let eos = self.best_nodes.len();
        self.best_nodes.push(BestNode {
            candidate: Candidate {
                surface: sentence_len as u32,
                end: last_filled_end as u32 + 1,
                source: NodeSource::Eos,
                ..bos_candidate
            },
            prev: None,
            cost: 0,
        });
        self.connect_best(dictionary, last_filled_end, eos);
        self.collect_best_nodes();
    }

    fn connect_best(&mut self, dictionary: &Dictionary, position: usize, first: usize) {
        let Self {
            best_nodes, ends, ..
        } = self;
        let (head, later_ends) = ends.split_at_mut(position + 1);
        let lefts: &[LeftCandidate] = &head[position];
        match &dictionary.matrix {
            Matrix::PackedDense(dense) => {
                for index in (first..best_nodes.len()).rev() {
                    let column = dense.right_column(best_nodes[index].candidate.left_id);
                    let (cost, best) = best_left::<false>(lefts, |id| {
                        debug_assert!(usize::from(id) < column.len());
                        unsafe { *column.get_unchecked(usize::from(id)) }
                    });
                    store_best(best_nodes, later_ends, position, index, cost, best);
                }
            }
            Matrix::PackedHot(hot) => {
                let hot_left = hot.hot_left();
                let hot = hot.view();
                let lefts_are_hot = lefts
                    .iter()
                    .all(|left| usize::from(left.right_id) < hot_left);
                if lefts_are_hot {
                    for node in &best_nodes[first..] {
                        let cells = hot.right_column(node.candidate.left_id).cells();
                        for left in lefts {
                            prefetch(cells.as_ptr().wrapping_add(usize::from(left.right_id)));
                        }
                    }
                }
                let mut next = best_nodes.len();
                while next > first {
                    if lefts_are_hot && next - first >= CONNECT_GROUP_SIZE {
                        let indices: [usize; CONNECT_GROUP_SIZE] =
                            std::array::from_fn(|lane| next - 1 - lane);
                        let columns = indices.map(|index| {
                            hot.right_column(best_nodes[index].candidate.left_id)
                                .cells()
                        });
                        if columns.iter().all(|cells| cells.len() == hot_left) {
                            let found = best_lefts(lefts, columns);
                            for (index, (cost, best)) in indices.into_iter().zip(found) {
                                store_best(best_nodes, later_ends, position, index, cost, best);
                            }
                            next -= CONNECT_GROUP_SIZE;
                            continue;
                        }
                    }
                    next -= 1;
                    let column = hot.right_column(best_nodes[next].candidate.left_id);
                    let cells = column.cells();
                    let (cost, best) = if lefts_are_hot && cells.len() == hot_left {
                        let cost = |id: u16| unsafe { *cells.get_unchecked(usize::from(id)) };
                        best_left::<true>(lefts, cost)
                    } else {
                        best_left::<true>(lefts, column.into_cost_fn())
                    };
                    store_best(best_nodes, later_ends, position, next, cost, best);
                }
            }
        }
    }

    fn collect_best_nodes(&mut self) {
        let eos = self.best_nodes.len() - 1;
        self.path_cost = i64::from(self.best_nodes[eos].cost);
        let mut count = 0;
        let mut cursor = Some(NodeId::from_index(eos));
        while let Some(id) = cursor {
            count += 1;
            cursor = self.best_nodes[id].prev;
        }
        self.nodes
            .resize(count, RawNode::boundary(NodeSource::Bos, 0));
        self.path.extend((0..count).map(NodeId::from_index));
        let mut cursor = Some(NodeId::from_index(eos));
        while let Some(id) = cursor {
            count -= 1;
            let best = self.best_nodes[id];
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
            node.prev = count.checked_sub(1).map(NodeId::from_index);
            self.nodes[count] = node;
            cursor = best.prev;
        }
    }

    pub(crate) fn nodes(&self) -> &[RawNode] {
        &self.nodes
    }

    pub(crate) fn ending_at(&self, position: usize) -> &[LeftCandidate] {
        self.ends.get(position).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn marginal(&self, index: usize) -> Option<(f32, f32, f32)> {
        let marginals = &self.marginals;
        marginals.ready.then(|| {
            (
                marginals.alpha[index],
                marginals.beta[index],
                marginals.prob[index],
            )
        })
    }

    pub(crate) fn z(&self) -> Option<f64> {
        self.marginals.ready.then_some(self.marginals.z)
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

    fn advance_nbest(&mut self) -> Option<ElementId> {
        while let Some(top) = self.agenda.pop() {
            let node_id = self.agenda.node(top);
            let node = &self.nodes[node_id];
            if node.source == NodeSource::Bos {
                return Some(top);
            }
            for link_index in node.links.range().rev() {
                let PathLink { left_node, cost } = self.links[link_index];
                let left_cost = self.nodes[left_node].cost;
                self.agenda.extend(top, left_node, left_cost, cost);
            }
        }
        None
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
    ) {
        let mut lefts = std::mem::take(&mut self.ends[position]);
        self.connect_rights::<NBEST>(dictionary, position, begun, &lefts);
        let added = &mut self.ends[position];
        if !added.is_empty() {
            lefts.append(added);
        }
        std::mem::swap(added, &mut lefts);
    }

    fn connect_rights<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        position: usize,
        begun: Range<usize>,
        lefts: &[LeftCandidate],
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
                let hot_left = hot.hot_left();
                let hot = hot.view();
                let lefts_are_hot = lefts
                    .iter()
                    .all(|left| usize::from(left.right_id) < hot_left);
                for right_index in newest_first(begun) {
                    let right_id = NodeId::from_index(right_index);
                    let column = hot.right_column(self.nodes[right_id].left_id);
                    let cells = column.cells();
                    if lefts_are_hot && cells.len() == hot_left {
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

    #[inline(always)]
    fn lookup<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
        view: &PackedView<'_>,
        unknown: &PackedDictionary,
        sentence: &[u8],
        begin: usize,
    ) -> Result<Range<usize>, Error> {
        let chars = &dictionary.chars;
        let sentence_len = sentence.len();
        let first = self.node_count::<BEST>();
        let mut end = sentence_len;
        let partial = !BEST && self.constraints.is_active();
        if partial
            && let Some(boundary) = (begin + 1..sentence_len)
                .find(|&position| self.constraints.is_token_boundary(position))
        {
            end = boundary;
        }
        let whole = end == sentence_len;
        let (surface, scan) = if whole {
            self.sentence_chars.skip_space(begin, chars.space())
        } else {
            chars.skip_space(sentence, begin, end)
        };
        if surface == sentence_len {
            return Ok(first..first);
        }
        let char_info = scan.info;
        let key_end = if end == surface { sentence_len } else { end };
        let key = &sentence[surface..key_end];
        let found = HitSite {
            begin,
            surface,
            partial,
            char_type: char_info.default_type() as u8,
            limit: end,
        };
        view.common_prefix_search(key, &mut self.hits);
        self.push_packed_hits::<BEST>(
            dictionary,
            system,
            view,
            NodeSource::System,
            sentence,
            &found,
        );
        for (index, user) in dictionary.users.iter().enumerate() {
            user.common_prefix_search(key, &mut self.hits);
            self.push_system_hits::<BEST>(
                dictionary,
                user,
                NodeSource::User(index as u8),
                sentence,
                &found,
            );
        }
        if self.node_count::<BEST>() > first && !char_info.invoke() {
            return Ok(first..self.node_count::<BEST>());
        }
        let mut after_char = surface + scan.width;
        let mut group_end: Option<usize> = None;
        if after_char > end {
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
            if self.node_count::<BEST>() > first {
                return Ok(first..self.node_count::<BEST>());
            }
        }
        if char_info.group() {
            let saved = after_char;
            let (grouped_end, group_scan) = if whole {
                self.sentence_chars.seek_other_type(after_char, char_info)
            } else {
                chars.seek_other_type(sentence, after_char, end, char_info)
            };
            after_char = grouped_end;
            if group_scan.count <= dictionary.max_grouping_size {
                let span = Span {
                    begin,
                    surface,
                    end: after_char,
                };
                self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
            }
            group_end = Some(after_char);
            after_char = saved;
        }
        for _ in 1..=char_info.length() {
            if after_char > end {
                break;
            }
            if Some(after_char) == group_end {
                continue;
            }
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
            let (next_info, width) = if whole {
                self.sentence_chars.char_info(after_char)
            } else {
                chars.char_info(sentence, after_char, end)
            };
            if !char_info.is_kind_of(next_info) {
                break;
            }
            after_char += width;
        }
        if self.node_count::<BEST>() == first {
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
        }
        if partial && self.node_count::<BEST>() == first {
            self.add_constrained_unknown(dictionary, unknown, sentence, begin, surface, end)?;
        }
        Ok(first..self.node_count::<BEST>())
    }

    #[inline]
    fn push_packed_hits<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        words: &PackedDictionary,
        view: &PackedView<'_>,
        source: NodeSource,
        sentence: &[u8],
        found: &HitSite,
    ) {
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            let end =
                dictionary
                    .chars
                    .extended_end(sentence, found.surface + hit.length(), found.limit);
            let source = if end == found.surface + hit.length() {
                source
            } else {
                NodeSource::SystemExtended
            };
            for token in view.tokens(hit.tokens()) {
                if !BEST
                    && found.partial
                    && !self.constraints.admits(found.begin, end, |out| {
                        let key = &sentence[found.surface..found.surface + hit.length()];
                        words.write_feature(token.feature, str::from_utf8(key).unwrap_or(""), out);
                    })
                {
                    continue;
                }
                let candidate = Candidate {
                    surface: found.surface as u32,
                    end: end as u32,
                    feature: token.feature,
                    left_id: token.left_id,
                    right_id: token.right_id,
                    pos_id: token.pos_id,
                    word_cost: token.word_cost,
                    source,
                    char_type: found.char_type,
                };
                self.push_candidate::<BEST>(found.begin, candidate);
            }
        }
    }

    #[inline]
    fn push_system_hits<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        words: &SystemDictionary,
        source: NodeSource,
        sentence: &[u8],
        found: &HitSite,
    ) {
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            let end =
                dictionary
                    .chars
                    .extended_end(sentence, found.surface + hit.length(), found.limit);
            for token_index in hit.tokens() {
                let token = words.token(token_index);
                if !BEST
                    && found.partial
                    && !self.constraints.admits(found.begin, end, |out| {
                        out.push_str(words.feature(token.feature))
                    })
                {
                    continue;
                }
                let candidate = Candidate {
                    surface: found.surface as u32,
                    end: end as u32,
                    feature: token.feature,
                    left_id: token.left_id,
                    right_id: token.right_id,
                    pos_id: token.pos_id,
                    word_cost: token.word_cost,
                    source,
                    char_type: found.char_type,
                };
                self.push_candidate::<BEST>(found.begin, candidate);
            }
        }
    }

    fn add_constrained_unknown(
        &mut self,
        dictionary: &Dictionary,
        unknown: &PackedDictionary,
        sentence: &[u8],
        begin: usize,
        surface: usize,
        end: usize,
    ) -> Result<(), Error> {
        let first = self.nodes.len();
        let mut cursor = surface;
        let char_info = loop {
            let (info, width) = dictionary.chars.char_info(sentence, cursor, end);
            cursor += width;
            if cursor > end || !self.constraints.is_inside_token(cursor) {
                break info;
            }
        };
        let span = Span {
            begin,
            surface,
            end: cursor,
        };
        self.add_unknown::<false>(dictionary, unknown, char_info, &span);
        if self.nodes.len() > first {
            return Ok(());
        }
        let Some(feature) = self.constraints.feature_at(begin) else {
            return Err(Error::UnsatisfiableConstraint);
        };
        self.push_node(RawNode {
            surface_start: surface as u32,
            length: (cursor - surface) as u32,
            length_with_space: (cursor - begin) as u32,
            left_id: 0,
            right_id: 0,
            word_cost: 0,
            pos_id: 0,
            source: NodeSource::Constrained,
            char_type: char_info.default_type() as u8,
            feature,
            cost: 0,
            prev: None,
            links: LinkSpan::default(),
        });
        Ok(())
    }

    #[inline(always)]
    fn add_unknown<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        unknown: &PackedDictionary,
        char_info: CharInfo,
        span: &Span,
    ) {
        let tokens = &dictionary.unknown_tokens[char_info.default_type()];
        let source = if dictionary.unknown_feature.is_some() {
            NodeSource::UnknownFixed
        } else {
            NodeSource::Unknown
        };
        for &token in tokens {
            if !BEST
                && self.constraints.is_active()
                && !self.constraints.admits(span.begin, span.end, |out| {
                    match &dictionary.unknown_feature {
                        Some(feature) => out.push_str(feature),
                        None => unknown.write_feature(token.feature, "", out),
                    }
                })
            {
                continue;
            }
            let candidate = Candidate {
                surface: span.surface as u32,
                end: span.end as u32,
                feature: token.feature,
                left_id: token.left_id,
                right_id: token.right_id,
                pos_id: token.pos_id,
                word_cost: token.word_cost,
                source,
                char_type: char_info.default_type() as u8,
            };
            self.push_candidate::<BEST>(span.begin, candidate);
        }
    }
}
