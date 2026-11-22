use super::{NodeId, Path, PathId, Sentence};
use crate::learn::features::ids_from;
use crate::learn::tokenizer::{BOS, EOS};

const WORST_COST: f64 = -1e37;

#[inline]
fn accumulate_alpha(start: f64, ids: impl Iterator<Item = usize>, alpha: &[f64]) -> f64 {
    ids.fold(start, |sum, id| sum + alpha[id])
}

#[inline]
fn add_weight(expected: &mut [f64], ids: impl Iterator<Item = usize>, weight: f64) {
    for id in ids {
        expected[id] += weight;
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

impl Sentence {
    fn is_empty(&self, path: &Path) -> bool {
        let right = &self.nodes[path.rnode.index()];
        let left = &self.nodes[path.lnode.index()];
        (right.rpath == PathId::NIL && right.kind != EOS)
            || (left.lpath == PathId::NIL && left.kind != BOS)
    }

    #[inline]
    fn word_cost(&self, node: NodeId, pool: &[i32], alpha: &[f64]) -> f64 {
        let record = &self.nodes[node.index()];
        if record.kind == EOS {
            0.0
        } else {
            accumulate_alpha(0.0, ids_from(pool, record.fvector), alpha)
        }
    }

    #[inline]
    fn refresh_path_cost(&mut self, path: PathId, pool: &[i32], alpha: &[f64]) {
        if self.is_empty(&self.paths[path.index()]) {
            return;
        }
        let record = &self.paths[path.index()];
        let start = self.nodes[record.rnode.index()].wcost;
        let cost = accumulate_alpha(start, ids_from(pool, record.fvector), alpha);
        self.paths[path.index()].cost = cost;
    }

    fn best_left(&mut self, node: NodeId, pool: &[i32], alpha: &[f64]) -> (NodeId, f64) {
        let mut best_cost = WORST_COST;
        let mut best = NodeId::NIL;
        let mut path = self.nodes[node.index()].lpath;
        while path != PathId::NIL {
            self.refresh_path_cost(path, pool, alpha);
            let record = &self.paths[path.index()];
            let cost = record.cost + self.nodes[record.lnode.index()].cost;
            if cost > best_cost {
                best_cost = cost;
                best = record.lnode;
            }
            path = record.lnext;
        }
        (best, best_cost)
    }

    fn score_node(&mut self, node: NodeId, pool: &[i32], alpha: &[f64]) {
        let wcost = self.word_cost(node, pool, alpha);
        self.nodes[node.index()].wcost = wcost;
        let (best, best_cost) = self.best_left(node, pool, alpha);
        let node_ref = &mut self.nodes[node.index()];
        node_ref.prev = best;
        node_ref.cost = best_cost;
    }

    fn mark_best_path(&mut self) {
        let mut node = self.begin_list[self.length];
        while self.nodes[node.index()].prev != NodeId::NIL {
            let prev = self.nodes[node.index()].prev;
            self.nodes[prev.index()].next = node;
            node = prev;
        }
    }

    fn viterbi(&mut self, pool: &[i32], alpha: &[f64]) {
        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NodeId::NIL {
                self.score_node(node, pool, alpha);
                node = self.nodes[node.index()].bnext;
            }
        }
        self.mark_best_path();
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

    fn accumulate_path(&self, record: &Path, pool: &[i32], z: f64, expected: &mut [f64]) {
        let left = &self.nodes[record.lnode.index()];
        let right = &self.nodes[record.rnode.index()];
        let weight = (left.alpha + record.cost + right.beta - z).exp();
        add_weight(expected, ids_from(pool, record.fvector), weight);
        if right.kind != EOS {
            add_weight(expected, ids_from(pool, right.fvector), weight);
        }
    }

    fn accumulate_node(&self, node: NodeId, pool: &[i32], z: f64, expected: &mut [f64]) {
        let mut path = self.nodes[node.index()].lpath;
        while path != PathId::NIL {
            let record = &self.paths[path.index()];
            if !self.is_empty(record) {
                self.accumulate_path(record, pool, z, expected);
            }
            path = record.lnext;
        }
    }

    fn accumulate_expected(&self, pool: &[i32], z: f64, expected: &mut [f64]) {
        for position in 0..=self.length {
            let mut node = self.begin_list[position];
            while node != NodeId::NIL {
                self.accumulate_node(node, pool, z, expected);
                node = self.nodes[node.index()].bnext;
            }
        }
    }

    pub(in crate::learn) fn gradient(
        &mut self,
        pool: &[i32],
        alpha: &[f64],
        expected: &mut [f64],
    ) -> f64 {
        self.viterbi(pool, alpha);
        self.forward();
        self.backward();
        let z = self.nodes[self.begin_list[self.length].index()].alpha;
        self.accumulate_expected(pool, z, expected);
        self.answer
            .iter()
            .fold(z, |total, &path| total - self.paths[path.index()].cost)
    }
}
