use std::io::Write;
use std::path::{Path, PathBuf};

use super::features::{FeatureIndex, Features};
use super::lattice::{Reader, Sentence};
use super::lbfgs::Lbfgs;
use super::tokenizer::Tokenizer;
use super::{Config, failure, format_g, open_lines};
use mecab_rs::Error;

const MAX_THREADS: usize = 512;
const PROGRESS_INTERVAL: usize = 100;
const CONVERGED_ITERATIONS: usize = 3;

#[derive(Clone, Debug)]
pub struct TrainOptions {
    pub dic_dir: PathBuf,
    pub old_model: Option<PathBuf>,
    pub cost: f64,
    pub freq: usize,
    pub eta: f64,
    pub threads: usize,
    pub eval_size: Option<usize>,
    pub unk_eval_size: Option<usize>,
    pub crlf: bool,
}

impl Default for TrainOptions {
    fn default() -> Self {
        Self {
            dic_dir: PathBuf::from("."),
            old_model: None,
            cost: 1.0,
            freq: 1,
            eta: 0.00005,
            threads: 1,
            eval_size: None,
            unk_eval_size: None,
            crlf: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrainReport {
    pub sentences: usize,
    pub features: usize,
    pub iterations: usize,
    pub objective: f64,
    pub charset: String,
}

#[derive(Default)]
struct Partial {
    expected: Vec<f64>,
    objective: f64,
    errors: usize,
    correct: usize,
    precision: usize,
    recall: usize,
}

struct Shared<'a> {
    pool: &'a [i32],
    alpha: &'a [f64],
    features: &'a Features,
    eval_size: usize,
    unk_eval_size: usize,
}

fn run_group(group: &mut [Sentence], shared: &Shared, partial: &mut Partial) {
    partial.correct = 0;
    partial.precision = 0;
    partial.recall = 0;
    partial.errors = 0;
    partial.objective = 0.0;
    partial.expected.fill(0.0);
    for sentence in group {
        partial.objective += sentence.gradient(shared.pool, shared.alpha, &mut partial.expected);
        partial.errors += sentence.eval(
            shared.features,
            shared.eval_size,
            shared.unk_eval_size,
            &mut partial.correct,
            &mut partial.precision,
            &mut partial.recall,
        );
    }
}

pub fn train(
    options: &TrainOptions,
    corpus: &Path,
    model: &Path,
    log: &mut dyn Write,
) -> Result<TrainReport, Error> {
    let dic_dir = options.dic_dir.as_path();
    let mut config = Config::default();
    config.set("cost", &options.cost.to_string(), true);
    config.set("eta", &options.eta.to_string(), true);
    config.set("freq", &options.freq.to_string(), true);
    config.set("thread", &options.threads.to_string(), true);
    if let Some(size) = options.eval_size {
        config.set("eval-size", &size.to_string(), true);
    }
    if let Some(size) = options.unk_eval_size {
        config.set("unk-eval-size", &size.to_string(), true);
    }
    config.load(&dic_dir.join("dicrc"))?;

    let mut tokenizer = Tokenizer::open(dic_dir, &config)?;
    let mut features = Features::open(dic_dir)?;
    let mut index = FeatureIndex::open(dic_dir)?;

    let mut old_alpha = Vec::new();
    if let Some(old_model) = &options.old_model {
        let _ = writeln!(log, "Using previous model: {}", old_model.display());
        let _ = writeln!(log, "--cost --freq and --eta options are overwritten.");
        let (alpha, settings) = index.reopen(old_model, &tokenizer.charset)?;
        old_alpha = alpha;
        for (key, value) in settings {
            config.set(&key, &value, true);
        }
    }

    let cost: f64 = config.number("cost");
    let eta: f64 = config.number("eta");
    let eval_size: usize = config.number("eval-size");
    let unk_eval_size: usize = config.number("unk-eval-size");
    let threads: usize = config.number("thread");
    let freq: usize = config.number("freq");

    if cost.is_nan() || cost <= 0.0 {
        return Err(failure(
            dic_dir,
            format!("cost parameter is out of range: {}", format_g(cost)),
        ));
    }
    if eta.is_nan() || eta <= 0.0 {
        return Err(failure(
            dic_dir,
            format!("eta is out of range: {}", format_g(eta)),
        ));
    }
    if eval_size == 0 {
        return Err(failure(dic_dir, "eval-size is out of range: 0"));
    }
    if unk_eval_size == 0 {
        return Err(failure(dic_dir, "unk-eval-size is out of range: 0"));
    }
    if freq == 0 {
        return Err(failure(
            dic_dir,
            format!("freq is out of range: {unk_eval_size}"),
        ));
    }
    if threads == 0 || threads > MAX_THREADS {
        return Err(failure(dic_dir, format!("# thread is invalid: {threads}")));
    }

    let _ = write!(log, "reading corpus ...");
    let mut lines = open_lines(corpus)?;
    let mut observed: Vec<f64> = Vec::new();
    let mut sentences: Vec<Sentence> = Vec::new();
    {
        let mut reader = Reader {
            tokenizer: &mut tokenizer,
            features: &mut features,
            index: &mut index,
            eval_size,
            unk_eval_size,
            found: Vec::new(),
        };
        loop {
            let sentence = Sentence::read(&mut reader, &mut lines, &mut observed, log)?;
            let done = sentence.is_none();
            sentences.extend(sentence);
            if sentences.len().is_multiple_of(PROGRESS_INTERVAL) {
                let _ = write!(log, "{}... ", sentences.len());
            }
            if done {
                break;
            }
        }
    }

    index.shrink(freq, &mut observed);
    index.clear_cache();
    features.clear_rewrites();

    let size = index.size();
    observed.resize(size, 0.0);
    let mut expected = vec![0.0; size];
    old_alpha.resize(size, 0.0);
    let mut alpha = old_alpha.clone();

    let _ = writeln!(log);
    let _ = writeln!(log, "Number of sentences: {}", sentences.len());
    let _ = writeln!(log, "Number of features:  {size}");
    let _ = writeln!(log, "eta:                 {eta:.5}");
    let _ = writeln!(log, "freq:                {freq}");
    let _ = writeln!(log, "eval-size:           {eval_size}");
    let _ = writeln!(log, "unk-eval-size:       {unk_eval_size}");
    let _ = writeln!(log, "threads:             {threads}");
    let _ = writeln!(log, "charset:             {}", tokenizer.charset);
    let _ = writeln!(log, "C(sigma^2):          {cost:.5}\n");

    let sentence_count = sentences.len();
    let mut groups: Vec<Vec<Sentence>> = (0..threads).map(|_| Vec::new()).collect();
    for (at, sentence) in sentences.into_iter().enumerate() {
        groups[at % threads].push(sentence);
    }
    let mut partials: Vec<Partial> = (0..threads)
        .map(|_| Partial {
            expected: vec![0.0; size],
            ..Partial::default()
        })
        .collect();

    let mut stable_iterations = 0;
    let mut previous = 0.0;
    let mut lbfgs = Lbfgs::default();
    let mut iterations = 0;
    let mut objective_value = 0.0;

    for iteration in 0.. {
        let mut objective = 0.0;
        let mut errors = 0;
        let mut micro_p = 0usize;
        let mut micro_r = 0usize;
        let mut micro_c = 0usize;

        let shared = Shared {
            pool: index.pool(),
            alpha: &alpha,
            features: &features,
            eval_size,
            unk_eval_size,
        };
        if threads > 1 {
            std::thread::scope(|scope| {
                for (group, partial) in groups.iter_mut().zip(partials.iter_mut()) {
                    let shared = &shared;
                    scope.spawn(move || run_group(group, shared, partial));
                }
            });
            expected.fill(0.0);
            for partial in &partials {
                objective += partial.objective;
                errors += partial.errors;
                micro_r += partial.recall;
                micro_p += partial.precision;
                micro_c += partial.correct;
                for (total, value) in expected.iter_mut().zip(&partial.expected) {
                    *total += value;
                }
            }
        } else {
            let partial = &mut partials[0];
            run_group(&mut groups[0], &shared, partial);
            expected.copy_from_slice(&partial.expected);
            objective = partial.objective;
            errors = partial.errors;
            micro_r = partial.recall;
            micro_p = partial.precision;
            micro_c = partial.correct;
        }

        let precision = 1.0 * micro_c as f64 / micro_p as f64;
        let recall = 1.0 * micro_c as f64 / micro_r as f64;
        let micro_f = 2.0 * precision * recall / (precision + recall);

        for i in 0..size {
            let penalty = alpha[i] - old_alpha[i];
            objective += penalty * penalty / (2.0 * cost);
            expected[i] = expected[i] - observed[i] + penalty / cost;
        }

        let diff = if iteration == 0 {
            1.0
        } else {
            (1.0 * (previous - objective)).abs() / previous
        };
        let _ = writeln!(
            log,
            "iter={iteration} err={:.5} F={micro_f:.5} target={objective:.5} diff={diff:.5}",
            1.0 * errors as f64 / sentence_count as f64
        );
        previous = objective;
        iterations = iteration + 1;
        objective_value = objective;

        if diff < eta {
            stable_iterations += 1;
        } else {
            stable_iterations = 0;
        }
        if stable_iterations == CONVERGED_ITERATIONS {
            break;
        }

        let status = lbfgs.optimize(size, &mut alpha, objective, &expected);
        if status < 0 {
            return Err(failure(dic_dir, "unexpected error in LBFGS routine"));
        }
        if status == 0 {
            break;
        }
    }

    let _ = writeln!(log, "\nDone! writing model file ... ");
    let header = format!(
        "eta: {}\nfreq: {freq}\nC: {}\neval-size: {eval_size}\nunk-eval-size: {unk_eval_size}\ncharset: {}\n",
        format_g(eta),
        format_g(cost),
        tokenizer.charset
    );
    index.save(model, &header, &alpha, options.crlf)?;

    Ok(TrainReport {
        sentences: sentence_count,
        features: size,
        iterations,
        objective: objective_value,
        charset: tokenizer.charset,
    })
}
