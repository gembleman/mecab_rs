use std::cmp::Ordering;

use super::answer::same_node;
use super::{EvalSizes, Node, NodeId, Sentence, Tally};
use crate::learn::features::{Features, widen};
use crate::learn::tokenizer::UNKNOWN;

struct Alignment {
    result: NodeId,
    answer: NodeId,
    result_end: usize,
    answer_end: usize,
}

impl Sentence {
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

    pub(in crate::learn) fn eval(
        &self,
        features: &Features,
        sizes: EvalSizes,
        tally: &mut Tally,
    ) -> usize {
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
