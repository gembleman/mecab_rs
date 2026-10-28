use super::*;

#[derive(Default)]
pub(super) struct Marginals {
    pub(super) ready: bool,
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

    pub(super) fn forward_backward(&mut self, theta: f32) {
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

    pub(super) fn advance_nbest(&mut self) -> Option<ElementId> {
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
}
