use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::ids::NodeId;
use crate::packed::PackedDictionary;
use crate::utf8::sequence_len;

use super::{
    BestNode, Candidate, EOS_END_PAST_LAST_FILLED, Lattice, LeftCandidate, MAX_TEXT_LEN,
    NodeSource, RawNode,
};

impl Lattice {
    pub(super) fn build<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        sentence: &[u8],
    ) -> Result<NodeId, Error> {
        self.build_with::<NBEST>(dictionary, &dictionary.system, sentence)
    }

    #[inline(always)]
    fn build_with<const NBEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
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
            let begun = self.lookup::<false>(dictionary, system, &view, sentence, position)?;
            self.connect::<NBEST>(dictionary, position, begun)?;
        }
        let last_filled_end = self.last_filled_end_slot(sentence_len);
        let eos_index = self.push_eos::<false>(sentence_len, 0);
        if let Some(position) = last_filled_end {
            self.connect::<NBEST>(dictionary, position, eos_index..eos_index + 1)?;
        }
        Ok(NodeId::from_index(eos_index))
    }

    pub(super) fn build_best(
        &mut self,
        dictionary: &Dictionary,
        sentence: &[u8],
    ) -> Result<(), Error> {
        self.build_best_with(dictionary, &dictionary.system, sentence)
    }

    #[inline(always)]
    fn build_best_with(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
        sentence: &[u8],
    ) -> Result<(), Error> {
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
                let found = self.lookup::<true>(dictionary, system, &view, sentence, position);
                debug_assert!(found.is_ok());
                self.connect_best(dictionary, position, first)?;
            }
            position += sequence_len(lead);
        }
        let last_filled_end = self.last_filled_end_slot(sentence_len).unwrap_or(0);
        let eos = self.push_eos::<true>(sentence_len, last_filled_end);
        self.connect_best(dictionary, last_filled_end, eos)?;
        self.collect_best_nodes();
        Ok(())
    }

    #[inline(always)]
    fn last_filled_end_slot(&self, sentence_len: usize) -> Option<usize> {
        (0..=sentence_len)
            .rev()
            .find(|&at| !self.ends[at].lefts.is_empty())
    }

    #[inline(always)]
    pub(super) fn push_bos<const BEST: bool>(&mut self) {
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
    pub(super) fn push_eos<const BEST: bool>(
        &mut self,
        sentence_len: usize,
        last_filled_end: usize,
    ) -> usize {
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

    pub(super) fn collect_best_nodes(&mut self) {
        let eos = NodeId::from_index(self.best_nodes.len() - 1);
        self.path_cost = i64::from(self.best_nodes[eos].cost);
        self.collect_best_ids(eos);
        let best_nodes = &self.best_nodes;
        let mut left_end = 0;
        self.nodes
            .extend(self.path.iter_mut().enumerate().map(|(slot, id)| {
                let best = best_nodes[*id];
                *id = NodeId::from_index(slot);
                let node = Self::materialize_node(best, left_end, slot);
                left_end = best.candidate.end;
                node
            }));
    }

    #[inline(always)]
    fn collect_best_ids(&mut self, eos: NodeId) {
        let mut cursor = Some(eos);
        while let Some(id) = cursor {
            self.path.push(id);
            cursor = self.best_nodes[id].prev;
        }
        self.path.reverse();
    }

    #[inline(always)]
    fn materialize_node(best: BestNode, left_end: u32, slot: usize) -> RawNode {
        let candidate = best.candidate;
        let mut node = match candidate.source {
            NodeSource::Bos | NodeSource::Eos => {
                RawNode::boundary(candidate.source, candidate.surface as usize)
            }
            _ => candidate.raw(left_end as usize),
        };
        node.cost = best.cost;
        node.prev = slot.checked_sub(1).map(NodeId::from_index);
        node
    }
}
