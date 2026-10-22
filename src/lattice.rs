use std::ops::Range;

use crate::char_property::{CharInfo, Scan};
use crate::constraint::{Constraint, ConstraintSet, NO_FEATURE};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::ids::{ElementId, LinkId, NodeId};
use crate::nbest::{Agenda, PathLink};
use crate::system_dictionary::PrefixHit;

const MAX_LOOKUP_BYTES: usize = 65535;
const LATTICE_SLACK: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeSource {
    System,
    Unknown,
    Constrained,
    Bos,
    Eos,
}

#[derive(Clone, Copy)]
pub(crate) struct RawNode {
    pub(crate) surface_start: u32,
    pub(crate) length: u16,
    pub(crate) rlength: u16,
    pub(crate) lc_attr: u16,
    pub(crate) rc_attr: u16,
    pub(crate) wcost: i16,
    pub(crate) pos_id: u16,
    pub(crate) source: NodeSource,
    pub(crate) char_type: u8,
    pub(crate) feature: u32,
    pub(crate) cost: i32,
    prev: Option<NodeId>,
    enext: Option<NodeId>,
    lpath: Option<LinkId>,
}

impl RawNode {
    fn boundary(source: NodeSource, surface_start: usize) -> Self {
        Self {
            surface_start: surface_start as u32,
            length: 0,
            rlength: 0,
            lc_attr: 0,
            rc_attr: 0,
            wcost: 0,
            pos_id: 0,
            source,
            char_type: 0,
            feature: 0,
            cost: 0,
            prev: None,
            enext: None,
            lpath: None,
        }
    }
}

#[derive(Default)]
struct Candidates {
    rc_attrs: Vec<u16>,
    costs: Vec<i32>,
    indices: Vec<NodeId>,
}

impl Candidates {
    fn gather(&mut self, nodes: &[RawNode], first: Option<NodeId>) {
        self.rc_attrs.clear();
        self.costs.clear();
        self.indices.clear();
        let mut left = first;
        while let Some(id) = left {
            let node = &nodes[id];
            self.rc_attrs.push(node.rc_attr);
            self.costs.push(node.cost);
            self.indices.push(id);
            left = node.enext;
        }
    }
}

struct Span {
    begin: usize,
    surface: usize,
    end: usize,
}

#[derive(Default)]
pub(crate) struct Lattice {
    nodes: Vec<RawNode>,
    end_nodes: Vec<Option<NodeId>>,
    hits: Vec<PrefixHit>,
    candidates: Candidates,
    path: Vec<NodeId>,
    constraints: ConstraintSet,
    all_paths: bool,
    links: Vec<PathLink>,
    agenda: Agenda,
}

impl Lattice {
    pub(crate) fn parse(
        &mut self,
        dictionary: &Dictionary,
        text: &str,
        constraints: &[Constraint<'_>],
        rank: usize,
    ) -> Result<(), Error> {
        if rank > 1 {
            self.start_nbest(dictionary, text, constraints)?;
            return self.select_nbest_path(rank);
        }
        self.reset(text, constraints, false);
        let eos = self.build::<false>(dictionary, text.as_bytes())?;
        let mut cursor = Some(eos);
        while let Some(id) = cursor {
            self.path.push(id);
            cursor = self.nodes[id].prev;
        }
        self.path.reverse();
        Ok(())
    }

    pub(crate) fn start_nbest(
        &mut self,
        dictionary: &Dictionary,
        text: &str,
        constraints: &[Constraint<'_>],
    ) -> Result<(), Error> {
        self.reset(text, constraints, true);
        let eos = self.build::<true>(dictionary, text.as_bytes())?;
        self.agenda.start(eos);
        Ok(())
    }

    pub(crate) fn next_nbest_path(&mut self) -> bool {
        self.path.clear();
        let Some(mut found) = self.advance_nbest() else {
            return false;
        };
        loop {
            self.path.push(self.agenda.node(found));
            match self.agenda.next(found) {
                Some(next) => found = next,
                None => return true,
            }
        }
    }

    fn reset(&mut self, text: &str, constraints: &[Constraint<'_>], all_paths: bool) {
        let sentence_len = text.len();
        self.nodes.clear();
        self.path.clear();
        self.links.clear();
        self.all_paths = all_paths;
        self.constraints.load(sentence_len, constraints);
        self.end_nodes.clear();
        self.end_nodes.resize(sentence_len + LATTICE_SLACK, None);
    }

    fn build<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        sentence: &[u8],
    ) -> Result<NodeId, Error> {
        let sentence_len = sentence.len();
        let bos = self.push_node(RawNode::boundary(NodeSource::Bos, 0));
        self.end_nodes[0] = Some(bos);
        for position in 0..sentence_len {
            if self.end_nodes[position].is_none() {
                continue;
            }
            let begun = self.lookup(dictionary, sentence, position)?;
            self.connect::<NBEST>(dictionary, position, begun)?;
        }
        let eos_index = self.nodes.len();
        let eos = self.push_node(RawNode::boundary(NodeSource::Eos, sentence_len));
        let last_end = (0..=sentence_len).rev().find(|&at| self.end_nodes[at].is_some());
        if let Some(position) = last_end {
            self.connect::<NBEST>(dictionary, position, eos_index..eos_index + 1)?;
        }
        Ok(eos)
    }

    pub(crate) fn nodes(&self) -> &[RawNode] {
        &self.nodes
    }

    pub(crate) fn path(&self) -> &[NodeId] {
        &self.path
    }

    pub(crate) fn constraints(&self) -> &ConstraintSet {
        &self.constraints
    }

    fn select_nbest_path(&mut self, rank: usize) -> Result<(), Error> {
        for _ in 0..rank {
            if !self.next_nbest_path() {
                return Err(Error::NoPath { rank });
            }
        }
        Ok(())
    }

    fn advance_nbest(&mut self) -> Option<ElementId> {
        while let Some(top) = self.agenda.pop() {
            let node_id = self.agenda.node(top);
            let node = &self.nodes[node_id];
            if node.source == NodeSource::Bos {
                return Some(top);
            }
            let mut link = node.lpath;
            while let Some(link_id) = link {
                let PathLink { left_node, cost, next_left } = self.links[link_id];
                let left_cost = self.nodes[left_node].cost;
                self.agenda.extend(top, left_node, left_cost, cost);
                link = next_left;
            }
        }
        None
    }

    fn push_node(&mut self, node: RawNode) -> NodeId {
        self.nodes.push(node);
        NodeId::from_index(self.nodes.len() - 1)
    }

    fn connect<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        position: usize,
        begun: Range<usize>,
    ) -> Result<(), Error> {
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.gather(&self.nodes, self.end_nodes[position]);
        for right_index in begun.rev() {
            let right_id = NodeId::from_index(right_index);
            let right_node = self.nodes[right_id];
            let column = dictionary.matrix.right_column(right_node.lc_attr);
            let word_cost = i32::from(right_node.wcost);
            let mut best_cost = i32::MAX;
            let mut best: Option<NodeId> = None;
            let mut lpath: Option<LinkId> = None;
            let lefts = candidates.rc_attrs.iter().zip(&candidates.costs).zip(&candidates.indices);
            for ((&rc_attr, &left_cost), &left) in lefts {
                let connection = column[usize::from(rc_attr)];
                let local = i32::from(connection) + word_cost;
                let cost = left_cost.wrapping_add(local);
                if cost < best_cost {
                    best = Some(left);
                    best_cost = cost;
                }
                if NBEST {
                    self.links.push(PathLink { left_node: left, cost: local, next_left: lpath });
                    lpath = Some(LinkId::from_index(self.links.len() - 1));
                }
            }
            let Some(best) = best else {
                self.candidates = candidates;
                return Err(Error::TooLongSentence);
            };
            let target = usize::from(right_node.rlength) + position;
            let Some(slot) = self.end_nodes.get_mut(target) else {
                self.candidates = candidates;
                return Err(Error::NodeEndOutOfRange);
            };
            let node = &mut self.nodes[right_id];
            node.prev = Some(best);
            node.cost = best_cost;
            node.lpath = lpath;
            node.enext = *slot;
            *slot = Some(right_id);
        }
        self.candidates = candidates;
        Ok(())
    }

    fn lookup(
        &mut self,
        dictionary: &Dictionary,
        sentence: &[u8],
        begin: usize,
    ) -> Result<Range<usize>, Error> {
        let chars = &dictionary.chars;
        let sentence_len = sentence.len();
        let first = self.nodes.len();
        let mut end = if sentence_len - begin >= MAX_LOOKUP_BYTES {
            begin + MAX_LOOKUP_BYTES
        } else {
            sentence_len
        };
        let partial = self.constraints.is_active();
        if partial
            && let Some(boundary) = (begin + 1..sentence_len)
                .find(|&position| self.constraints.is_token_boundary(position))
        {
            end = boundary;
        }
        let mut scan = Scan::default();
        let surface = chars.seek_other_type(sentence, begin, end, chars.space(), &mut scan);
        if surface == sentence_len {
            return Ok(first..first);
        }
        let char_info = scan.info;
        let key_end = if end == surface { sentence_len } else { end };
        dictionary.system.common_prefix_search(&sentence[surface..key_end], &mut self.hits);
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            for offset in 0..hit.token_count() {
                let token = dictionary.system.token(hit.first_token() + offset)?;
                if partial
                    && !self.constraints.admits(
                        begin,
                        surface + hit.length,
                        dictionary.system.feature(token.feature),
                    )
                {
                    continue;
                }
                let node = RawNode {
                    surface_start: surface as u32,
                    length: hit.length as u16,
                    rlength: (surface - begin + hit.length) as u16,
                    lc_attr: token.lc_attr,
                    rc_attr: token.rc_attr,
                    wcost: token.wcost,
                    pos_id: token.pos_id,
                    source: NodeSource::System,
                    char_type: char_info.default_type() as u8,
                    feature: token.feature,
                    cost: 0,
                    prev: None,
                    enext: None,
                    lpath: None,
                };
                self.push_node(node);
            }
        }
        if self.nodes.len() > first && !char_info.invoke() {
            return Ok(first..self.nodes.len());
        }
        let mut after_char = surface + scan.width;
        let mut group_end: Option<usize> = None;
        if after_char > end {
            let span = Span { begin, surface, end: after_char };
            self.add_unknown(dictionary, char_info, &span);
            if self.nodes.len() > first {
                return Ok(first..self.nodes.len());
            }
        }
        if char_info.group() {
            let saved = after_char;
            let mut group_scan = Scan::default();
            after_char =
                chars.seek_other_type(sentence, after_char, end, char_info, &mut group_scan);
            if group_scan.count <= dictionary.max_grouping_size {
                let span = Span { begin, surface, end: after_char };
                self.add_unknown(dictionary, char_info, &span);
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
            let span = Span { begin, surface, end: after_char };
            self.add_unknown(dictionary, char_info, &span);
            let (next_info, width) = chars.char_info(sentence, after_char, end);
            if !char_info.is_kind_of(next_info) {
                break;
            }
            after_char += width;
        }
        if self.nodes.len() == first {
            let span = Span { begin, surface, end: after_char };
            self.add_unknown(dictionary, char_info, &span);
        }
        if partial && self.nodes.len() == first {
            self.add_constrained_unknown(dictionary, sentence, begin, surface, end)?;
        }
        Ok(first..self.nodes.len())
    }

    fn add_constrained_unknown(
        &mut self,
        dictionary: &Dictionary,
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
        let span = Span { begin, surface, end: cursor };
        self.add_unknown(dictionary, char_info, &span);
        if self.nodes.len() > first {
            return Ok(());
        }
        let feature = self.constraints.feature_at(begin);
        if feature == NO_FEATURE {
            return Err(Error::UnsatisfiableConstraint);
        }
        self.push_node(RawNode {
            surface_start: surface as u32,
            length: (cursor - surface) as u16,
            rlength: (cursor - begin) as u16,
            lc_attr: 0,
            rc_attr: 0,
            wcost: 0,
            pos_id: 0,
            source: NodeSource::Constrained,
            char_type: char_info.default_type() as u8,
            feature,
            cost: 0,
            prev: None,
            enext: None,
            lpath: None,
        });
        Ok(())
    }

    fn add_unknown(&mut self, dictionary: &Dictionary, char_info: CharInfo, span: &Span) {
        let Some(tokens) = dictionary.unknown_tokens.get(char_info.default_type()) else {
            return;
        };
        for &token in tokens {
            if self.constraints.is_active()
                && !self.constraints.admits(
                    span.begin,
                    span.end,
                    dictionary.unknown.feature(token.feature),
                )
            {
                continue;
            }
            let node = RawNode {
                surface_start: span.surface as u32,
                length: (span.end - span.surface) as u16,
                rlength: (span.end - span.begin) as u16,
                lc_attr: token.lc_attr,
                rc_attr: token.rc_attr,
                wcost: token.wcost,
                pos_id: token.pos_id,
                source: NodeSource::Unknown,
                char_type: char_info.default_type() as u8,
                feature: token.feature,
                cost: 0,
                prev: None,
                enext: None,
                lpath: None,
            };
            self.push_node(node);
        }
    }
}
