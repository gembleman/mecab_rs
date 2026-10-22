use crate::char_property::{CharInfo, Scan};
use crate::constraint::{Constraint, ConstraintSet, NO_FEATURE};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::nbest::{Agenda, NIL, PathLink};
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
    prev: u32,
    enext: u32,
    bnext: u32,
    lpath: u32,
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
            prev: NIL,
            enext: NIL,
            bnext: NIL,
            lpath: NIL,
        }
    }
}

#[derive(Default)]
struct Candidates {
    rc_attrs: Vec<u16>,
    costs: Vec<i32>,
    indices: Vec<u32>,
}

impl Candidates {
    fn gather(&mut self, nodes: &[RawNode], first: u32) {
        self.rc_attrs.clear();
        self.costs.clear();
        self.indices.clear();
        let mut left = first;
        while left != NIL {
            let node = &nodes[left as usize];
            self.rc_attrs.push(node.rc_attr);
            self.costs.push(node.cost);
            self.indices.push(left);
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
    sentence: Vec<u8>,
    sentence_len: usize,
    nodes: Vec<RawNode>,
    begin_nodes: Vec<u32>,
    end_nodes: Vec<u32>,
    hits: Vec<PrefixHit>,
    candidates: Candidates,
    path: Vec<u32>,
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
        constraints: &[Constraint],
        rank: usize,
    ) -> Result<(), Error> {
        self.sentence.clear();
        self.sentence.extend_from_slice(text.as_bytes());
        self.sentence.push(0);
        self.sentence_len = text.len();
        self.nodes.clear();
        self.path.clear();
        self.links.clear();
        self.all_paths = rank > 1;
        self.constraints.load(text.len(), constraints);
        let slots = self.sentence_len + LATTICE_SLACK;
        self.begin_nodes.clear();
        self.begin_nodes.resize(slots, NIL);
        self.end_nodes.clear();
        self.end_nodes.resize(slots, NIL);
        let bos = self.push_node(RawNode::boundary(NodeSource::Bos, 0));
        self.end_nodes[0] = bos;
        for position in 0..self.sentence_len {
            if self.end_nodes[position] == NIL {
                continue;
            }
            let head = self.lookup(dictionary, position)?;
            self.begin_nodes[position] = head;
            self.connect(dictionary, position, head)?;
        }
        let eos = self.push_node(RawNode::boundary(NodeSource::Eos, self.sentence_len));
        self.begin_nodes[self.sentence_len] = eos;
        let last_end = (0..=self.sentence_len).rev().find(|&at| self.end_nodes[at] != NIL);
        if let Some(position) = last_end {
            self.connect(dictionary, position, eos)?;
        }
        if self.all_paths {
            return self.select_nbest_path(eos, rank);
        }
        let mut cursor = eos;
        while cursor != NIL {
            self.path.push(cursor);
            cursor = self.nodes[cursor as usize].prev;
        }
        self.path.reverse();
        Ok(())
    }

    pub(crate) fn nodes(&self) -> &[RawNode] {
        &self.nodes
    }

    pub(crate) fn path(&self) -> &[u32] {
        &self.path
    }

    pub(crate) fn constraints(&self) -> &ConstraintSet {
        &self.constraints
    }

    fn select_nbest_path(&mut self, eos: u32, rank: usize) -> Result<(), Error> {
        self.agenda.start(eos);
        let mut found = NIL;
        for _ in 0..rank {
            found = self.advance_nbest().ok_or(Error::NoPath { rank })?;
        }
        while found != NIL {
            self.path.push(self.agenda.node(found));
            found = self.agenda.next(found);
        }
        Ok(())
    }

    fn advance_nbest(&mut self) -> Option<u32> {
        while let Some(top) = self.agenda.pop() {
            let node_index = self.agenda.node(top);
            let node = &self.nodes[node_index as usize];
            if node.source == NodeSource::Bos {
                return Some(top);
            }
            let mut link = node.lpath;
            while link != NIL {
                let PathLink { left_node, cost, next_left } = self.links[link as usize];
                let left_cost = self.nodes[left_node as usize].cost;
                self.agenda.extend(top, left_node, left_cost, cost);
                link = next_left;
            }
        }
        None
    }

    fn push_node(&mut self, node: RawNode) -> u32 {
        self.nodes.push(node);
        (self.nodes.len() - 1) as u32
    }

    fn connect(&mut self, dictionary: &Dictionary, position: usize, head: u32) -> Result<(), Error> {
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.gather(&self.nodes, self.end_nodes[position]);
        let mut right = head;
        while right != NIL {
            let right_node = self.nodes[right as usize];
            let column = dictionary.matrix.right_column(right_node.lc_attr);
            let word_cost = i32::from(right_node.wcost);
            let mut best_cost = i32::MAX;
            let mut best = NIL;
            let mut lpath = NIL;
            let lefts = candidates.rc_attrs.iter().zip(&candidates.costs).zip(&candidates.indices);
            for ((&rc_attr, &left_cost), &left) in lefts {
                let connection = column.get(usize::from(rc_attr)).copied().unwrap_or(0);
                let local = i32::from(connection) + word_cost;
                let cost = left_cost.wrapping_add(local);
                if cost < best_cost {
                    best = left;
                    best_cost = cost;
                }
                if self.all_paths {
                    self.links.push(PathLink { left_node: left, cost: local, next_left: lpath });
                    lpath = (self.links.len() - 1) as u32;
                }
            }
            if best == NIL {
                self.candidates = candidates;
                return Err(Error::TooLongSentence);
            }
            let target = usize::from(right_node.rlength) + position;
            let Some(slot) = self.end_nodes.get_mut(target) else {
                self.candidates = candidates;
                return Err(Error::NodeEndOutOfRange);
            };
            let node = &mut self.nodes[right as usize];
            node.prev = best;
            node.cost = best_cost;
            node.lpath = lpath;
            node.enext = *slot;
            *slot = right;
            right = node.bnext;
        }
        self.candidates = candidates;
        Ok(())
    }

    fn lookup(&mut self, dictionary: &Dictionary, begin: usize) -> Result<u32, Error> {
        let chars = &dictionary.chars;
        let mut end = if self.sentence_len - begin >= MAX_LOOKUP_BYTES {
            begin + MAX_LOOKUP_BYTES
        } else {
            self.sentence_len
        };
        let partial = self.constraints.is_active();
        if partial
            && let Some(boundary) = (begin + 1..self.sentence_len)
                .find(|&position| self.constraints.is_token_boundary(position))
        {
            end = boundary;
        }
        let mut scan = Scan::default();
        let surface =
            chars.seek_other_type(&self.sentence, begin, end, chars.space(), &mut scan);
        if surface == self.sentence_len {
            return Ok(NIL);
        }
        let char_info = scan.info;
        let key_end = if end == surface { self.sentence_len } else { end };
        dictionary
            .system
            .common_prefix_search(&self.sentence[surface..key_end], &mut self.hits);
        let mut head = NIL;
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            for offset in 0..hit.token_count() {
                let token = dictionary.system.token(hit.first_token() + offset);
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
                    prev: NIL,
                    enext: NIL,
                    bnext: head,
                    lpath: NIL,
                };
                head = self.push_node(node);
            }
        }
        if head != NIL && !char_info.invoke() {
            return Ok(head);
        }
        let mut after_char = surface + scan.width;
        let mut group_end: Option<usize> = None;
        if after_char > end {
            let span = Span { begin, surface, end: after_char };
            head = self.add_unknown(dictionary, char_info, &span, head);
            if head != NIL {
                return Ok(head);
            }
        }
        if char_info.group() {
            let saved = after_char;
            let mut group_scan = Scan::default();
            after_char =
                chars.seek_other_type(&self.sentence, after_char, end, char_info, &mut group_scan);
            if group_scan.count <= dictionary.max_grouping_size {
                let span = Span { begin, surface, end: after_char };
                head = self.add_unknown(dictionary, char_info, &span, head);
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
            head = self.add_unknown(dictionary, char_info, &span, head);
            let (next_info, width) = chars.char_info(&self.sentence, after_char, end);
            if !char_info.is_kind_of(next_info) {
                break;
            }
            after_char += width;
        }
        if head == NIL {
            let span = Span { begin, surface, end: after_char };
            head = self.add_unknown(dictionary, char_info, &span, head);
        }
        if partial && head == NIL {
            head = self.add_constrained_unknown(dictionary, begin, surface, end)?;
        }
        Ok(head)
    }

    fn add_constrained_unknown(
        &mut self,
        dictionary: &Dictionary,
        begin: usize,
        surface: usize,
        end: usize,
    ) -> Result<u32, Error> {
        let mut cursor = surface;
        let char_info = loop {
            let (info, width) = dictionary.chars.char_info(&self.sentence, cursor, end);
            cursor += width;
            if cursor > end || !self.constraints.is_inside_token(cursor) {
                break info;
            }
        };
        let span = Span { begin, surface, end: cursor };
        let head = self.add_unknown(dictionary, char_info, &span, NIL);
        if head != NIL {
            return Ok(head);
        }
        let feature = self.constraints.feature_at(begin);
        if feature == NO_FEATURE {
            return Err(Error::UnsatisfiableConstraint);
        }
        Ok(self.push_node(RawNode {
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
            prev: NIL,
            enext: NIL,
            bnext: NIL,
            lpath: NIL,
        }))
    }

    fn add_unknown(
        &mut self,
        dictionary: &Dictionary,
        char_info: CharInfo,
        span: &Span,
        mut head: u32,
    ) -> u32 {
        let Some(&(first, count)) = dictionary.unknown_tokens.get(char_info.default_type()) else {
            return head;
        };
        for offset in 0..count {
            let token = dictionary.unknown.token(first + offset);
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
                prev: NIL,
                enext: NIL,
                bnext: head,
                lpath: NIL,
            };
            head = self.push_node(node);
        }
        head
    }
}
