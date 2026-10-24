use std::ops::Range;

use crate::char_property::{CharInfo, SentenceChars};
use crate::constraint::{Constraint, ConstraintSet};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::ids::{ElementId, NodeId};
use crate::matrix::Matrix;
use crate::nbest::{Agenda, LinkSpan, PathLink};
use crate::system_dictionary::{PrefixHit, SystemDictionary};

fn later_first<I: DoubleEndedIterator>(candidates: I) -> std::iter::Rev<I> {
    candidates.rev()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeSource {
    System,
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

struct Found {
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
        let m = &mut self.marginals;
        m.alpha.clear();
        m.alpha.resize(count, 0.0);
        m.beta.clear();
        m.beta.resize(count, 0.0);
        m.prob.clear();
        m.prob.resize(count, 0.0);
        m.seeded.clear();
        m.seeded.resize(count, false);
        for index in 1..count {
            let mut alpha = 0.0f32;
            for (step, link_index) in self.nodes[index].links.range().rev().enumerate() {
                let PathLink { left_node, cost } = self.links[link_index];
                let value = -theta * f64::from(cost) + f64::from(m.alpha[left_node]);
                alpha = if step == 0 {
                    value as f32
                } else {
                    logsumexp(f64::from(alpha), value) as f32
                };
            }
            m.alpha[index] = alpha;
        }
        let nodes = &self.nodes;
        let links = &self.links;
        let relax = |m: &mut Marginals, index: usize| {
            let right_beta = m.beta[index];
            for link_index in nodes[index].links.range().rev() {
                let PathLink { left_node, cost } = links[link_index];
                let value = -theta * f64::from(cost) + f64::from(right_beta);
                let left = left_node.index();
                m.beta[left] = if m.seeded[left] {
                    logsumexp(f64::from(m.beta[left]), value) as f32
                } else {
                    m.seeded[left] = true;
                    value as f32
                };
            }
        };
        let eos = count - 1;
        relax(m, eos);
        let mut end = eos;
        while end > 0 {
            let begin = nodes[end - 1].begin();
            let start = nodes[..end].partition_point(|raw| raw.begin() < begin);
            for index in start..end {
                relax(m, index);
            }
            end = start;
        }
        let z = f64::from(m.alpha[count - 1]);
        m.z = z;
        for index in 1..count {
            m.prob[index] = (f64::from(m.alpha[index] + m.beta[index]) - z).exp() as f32;
        }
        m.ready = true;
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
        self.path.clear();
        self.links.clear();
        self.marginals.ready = false;
        self.constraints.load(sentence_len, constraints);
        for ending in &mut self.ends[..self.ends_used] {
            ending.clear();
        }
        if self.ends.len() <= sentence_len {
            self.ends.resize_with(sentence_len + 1, Vec::new);
        }
        self.ends_used = sentence_len + 1;
    }

    fn build<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        sentence: &[u8],
    ) -> Result<NodeId, Error> {
        let sentence_len = sentence.len();
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
            let begun = self.lookup(dictionary, sentence, position)?;
            self.connect::<NBEST>(dictionary, position, begun);
        }
        let eos_index = self.nodes.len();
        let eos = self.push_node(RawNode::boundary(NodeSource::Eos, sentence_len));
        let last_end = (0..=sentence_len)
            .rev()
            .find(|&at| !self.ends[at].is_empty());
        if let Some(position) = last_end {
            self.connect::<NBEST>(dictionary, position, eos_index..eos_index + 1);
        }
        Ok(eos)
    }

    pub(crate) fn nodes(&self) -> &[RawNode] {
        &self.nodes
    }

    pub(crate) fn ending_at(&self, position: usize) -> &[LeftCandidate] {
        self.ends.get(position).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn marginal(&self, index: usize) -> Option<(f32, f32, f32)> {
        let m = &self.marginals;
        m.ready
            .then(|| (m.alpha[index], m.beta[index], m.prob[index]))
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
            Matrix::Dense(dense) => {
                for right_index in later_first(begun) {
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
            Matrix::Hot(hot) => {
                let hot_left = hot.hot_left();
                let lefts_are_hot = lefts
                    .iter()
                    .all(|left| usize::from(left.right_id) < hot_left);
                for right_index in later_first(begun) {
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
            for left in later_first(lefts.iter()) {
                let local = i32::from(connection_cost(left.right_id)) + word_cost;
                let cost = left.cost + local;
                if cost < best_cost {
                    best_cost = cost;
                    best = left.id;
                }
                self.links.push(PathLink {
                    left_node: left.id,
                    cost: local,
                });
            }
            (best_cost, best)
        } else {
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

    fn lookup(
        &mut self,
        dictionary: &Dictionary,
        sentence: &[u8],
        begin: usize,
    ) -> Result<Range<usize>, Error> {
        let chars = &dictionary.chars;
        let sentence_len = sentence.len();
        let first = self.nodes.len();
        let mut end = sentence_len;
        let partial = self.constraints.is_active();
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
        let found = Found {
            begin,
            surface,
            partial,
            char_type: char_info.default_type() as u8,
            limit: end,
        };
        dictionary
            .system
            .common_prefix_search(key, &mut self.hits)?;
        self.push_hits(
            dictionary,
            &dictionary.system,
            NodeSource::System,
            sentence,
            &found,
        );
        for (index, user) in dictionary.users.iter().enumerate() {
            user.common_prefix_search(key, &mut self.hits)?;
            self.push_hits(
                dictionary,
                user,
                NodeSource::User(index as u8),
                sentence,
                &found,
            );
        }
        if self.nodes.len() > first && !char_info.invoke() {
            return Ok(first..self.nodes.len());
        }
        let mut after_char = surface + scan.width;
        let mut group_end: Option<usize> = None;
        if after_char > end {
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown(dictionary, char_info, &span);
            if self.nodes.len() > first {
                return Ok(first..self.nodes.len());
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
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown(dictionary, char_info, &span);
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
        if self.nodes.len() == first {
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown(dictionary, char_info, &span);
        }
        if partial && self.nodes.len() == first {
            self.add_constrained_unknown(dictionary, sentence, begin, surface, end)?;
        }
        Ok(first..self.nodes.len())
    }

    #[inline]
    fn push_hits(
        &mut self,
        dictionary: &Dictionary,
        words: &SystemDictionary,
        source: NodeSource,
        sentence: &[u8],
        found: &Found,
    ) {
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            let end =
                dictionary
                    .chars
                    .extended_end(sentence, found.surface + hit.length, found.limit);
            let length = end - found.surface;
            for token_index in hit.tokens() {
                let token = words.token(token_index);
                if found.partial
                    && !self
                        .constraints
                        .admits(found.begin, end, || words.feature(token.feature))
                {
                    continue;
                }
                let node = RawNode {
                    surface_start: found.surface as u32,
                    length: length as u32,
                    length_with_space: (end - found.begin) as u32,
                    left_id: token.left_id,
                    right_id: token.right_id,
                    word_cost: token.word_cost,
                    pos_id: token.pos_id,
                    source,
                    char_type: found.char_type,
                    feature: token.feature,
                    cost: 0,
                    prev: None,
                    links: LinkSpan::default(),
                };
                self.push_node(node);
            }
        }
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
        let span = Span {
            begin,
            surface,
            end: cursor,
        };
        self.add_unknown(dictionary, char_info, &span);
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

    fn add_unknown(&mut self, dictionary: &Dictionary, char_info: CharInfo, span: &Span) {
        let Some(tokens) = dictionary.unknown_tokens.get(char_info.default_type()) else {
            return;
        };
        let source = if dictionary.unknown_feature.is_some() {
            NodeSource::UnknownFixed
        } else {
            NodeSource::Unknown
        };
        for &token in tokens {
            if self.constraints.is_active()
                && !self.constraints.admits(span.begin, span.end, || {
                    match &dictionary.unknown_feature {
                        Some(feature) => feature,
                        None => dictionary.unknown.feature(token.feature),
                    }
                })
            {
                continue;
            }
            let node = RawNode {
                surface_start: span.surface as u32,
                length: (span.end - span.surface) as u32,
                length_with_space: (span.end - span.begin) as u32,
                left_id: token.left_id,
                right_id: token.right_id,
                word_cost: token.word_cost,
                pos_id: token.pos_id,
                source,
                char_type: char_info.default_type() as u8,
                feature: token.feature,
                cost: 0,
                prev: None,
                links: LinkSpan::default(),
            };
            self.push_node(node);
        }
    }
}
