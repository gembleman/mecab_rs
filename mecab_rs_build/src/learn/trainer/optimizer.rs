use std::io::Write;
use std::path::Path;

use super::TrainSettings;
use crate::learn::features::Features;
use crate::learn::lattice::{EvalSizes, Sentence, Tally};
use crate::learn::lbfgs::Lbfgs;
use crate::tool_error::learn_error;
use mecab_rs::Error;

const CONVERGED_ITERATIONS: usize = 3;

#[derive(Default)]
struct Partial {
    expected: Vec<f64>,
    objective: f64,
    errors: usize,
    tally: Tally,
}

struct Shared<'a> {
    pool: &'a [i32],
    alpha: &'a [f64],
    features: &'a Features,
    sizes: EvalSizes,
}

fn run_group(group: &mut [Sentence], shared: &Shared, partial: &mut Partial) {
    partial.tally = Tally::default();
    partial.errors = 0;
    partial.objective = 0.0;
    partial.expected.fill(0.0);
    for sentence in group {
        partial.objective += sentence.gradient(shared.pool, shared.alpha, &mut partial.expected);
        partial.errors += sentence.eval(shared.features, shared.sizes, &mut partial.tally);
    }
}

fn run_groups(groups: &mut [Vec<Sentence>], partials: &mut [Partial], shared: &Shared) {
    if groups.len() > 1 {
        std::thread::scope(move |scope| {
            for (group, partial) in groups.iter_mut().zip(partials.iter_mut()) {
                scope.spawn(move || run_group(group, shared, partial));
            }
        });
    } else {
        for (group, partial) in groups.iter_mut().zip(partials.iter_mut()) {
            run_group(group, shared, partial);
        }
    }
}

fn split_into_groups(sentences: Vec<Sentence>, threads: usize) -> Vec<Vec<Sentence>> {
    let mut groups: Vec<Vec<Sentence>> = (0..threads).map(|_| Vec::new()).collect();
    for (at, sentence) in sentences.into_iter().enumerate() {
        groups[at % threads].push(sentence);
    }
    groups
}

#[derive(Default)]
struct Totals {
    objective: f64,
    errors: usize,
    tally: Tally,
}

impl Totals {
    fn absorb(&mut self, partial: &Partial) {
        self.objective += partial.objective;
        self.errors += partial.errors;
        self.tally.correct += partial.tally.correct;
        self.tally.precision += partial.tally.precision;
        self.tally.recall += partial.tally.recall;
    }
}

fn compute_micro_f(tally: &Tally) -> f64 {
    let precision = 1.0 * tally.correct as f64 / tally.precision as f64;
    let recall = 1.0 * tally.correct as f64 / tally.recall as f64;
    2.0 * precision * recall / (precision + recall)
}

fn ensure_finite(dic_dir: &Path, objective: f64, iteration: usize) -> Result<(), Error> {
    if objective.is_finite() {
        return Ok(());
    }
    Err(learn_error(
        dic_dir,
        format!("the objective became non-finite at iteration {iteration}"),
    ))
}

fn log_iteration(
    log: &mut dyn Write,
    iteration: usize,
    error_rate: f64,
    micro_f: f64,
    objective: f64,
    diff: f64,
) {
    let _ = writeln!(
        log,
        "iter={iteration} err={error_rate:.5} F={micro_f:.5} target={objective:.5} diff={diff:.5}"
    );
}

pub(super) struct Optimizer {
    cost: f64,
    eta: f64,
    sizes: EvalSizes,
    size: usize,
    pub(super) sentence_count: usize,
    groups: Vec<Vec<Sentence>>,
    partials: Vec<Partial>,
    expected: Vec<f64>,
    observed: Vec<f64>,
    old_alpha: Vec<f64>,
    pub(super) alpha: Vec<f64>,
    stable_iterations: usize,
    previous: f64,
    lbfgs: Lbfgs,
}

impl Optimizer {
    pub(super) fn new(
        settings: &TrainSettings,
        sentences: Vec<Sentence>,
        size: usize,
        observed: Vec<f64>,
        old_alpha: Vec<f64>,
    ) -> Self {
        let sentence_count = sentences.len();
        let threads = settings.threads;
        let alpha = old_alpha.clone();
        Self {
            cost: settings.cost,
            eta: settings.eta,
            sizes: settings.sizes(),
            size,
            sentence_count,
            groups: split_into_groups(sentences, threads),
            partials: (0..threads)
                .map(|_| Partial {
                    expected: vec![0.0; size],
                    ..Partial::default()
                })
                .collect(),
            expected: vec![0.0; size],
            observed,
            old_alpha,
            alpha,
            stable_iterations: 0,
            previous: 0.0,
            lbfgs: Lbfgs::default(),
        }
    }

    pub(super) fn run(
        &mut self,
        dic_dir: &Path,
        pool: &[i32],
        features: &Features,
        log: &mut dyn Write,
    ) -> Result<(), Error> {
        for iteration in 0.. {
            let totals = self.sweep(pool, features);
            let micro_f = compute_micro_f(&totals.tally);
            let objective = self.penalize(totals.objective);
            ensure_finite(dic_dir, objective, iteration)?;
            let diff = self.change(iteration, objective);
            let error_rate = 1.0 * totals.errors as f64 / self.sentence_count as f64;
            log_iteration(log, iteration, error_rate, micro_f, objective, diff);
            self.previous = objective;
            if self.has_converged(diff) {
                break;
            }
            if !self.advance(dic_dir, objective)? {
                break;
            }
        }
        Ok(())
    }

    fn sweep(&mut self, pool: &[i32], features: &Features) -> Totals {
        let shared = Shared {
            pool,
            alpha: &self.alpha,
            features,
            sizes: self.sizes,
        };
        run_groups(&mut self.groups, &mut self.partials, &shared);
        self.expected.fill(0.0);
        let mut totals = Totals::default();
        for partial in &self.partials {
            totals.absorb(partial);
            for (total, value) in self.expected.iter_mut().zip(&partial.expected) {
                *total += value;
            }
        }
        totals
    }

    fn penalize(&mut self, mut objective: f64) -> f64 {
        for i in 0..self.size {
            let penalty = self.alpha[i] - self.old_alpha[i];
            objective += penalty * penalty / (2.0 * self.cost);
            self.expected[i] = self.expected[i] - self.observed[i] + penalty / self.cost;
        }
        objective
    }

    fn change(&self, iteration: usize, objective: f64) -> f64 {
        if iteration == 0 {
            1.0
        } else {
            (1.0 * (self.previous - objective)).abs() / self.previous
        }
    }

    fn has_converged(&mut self, diff: f64) -> bool {
        if diff < self.eta {
            self.stable_iterations += 1;
        } else {
            self.stable_iterations = 0;
        }
        self.stable_iterations == CONVERGED_ITERATIONS
    }

    fn advance(&mut self, dic_dir: &Path, objective: f64) -> Result<bool, Error> {
        let status = self
            .lbfgs
            .optimize(self.size, &mut self.alpha, objective, &self.expected);
        if status < 0 {
            return Err(learn_error(dic_dir, "unexpected error in LBFGS routine"));
        }
        Ok(status != 0)
    }
}
