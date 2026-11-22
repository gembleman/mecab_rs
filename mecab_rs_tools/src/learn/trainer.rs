use std::io::Write;
use std::path::{Path, PathBuf};

use self::optimizer::Optimizer;
use super::features::{FeatureIndex, Features, remap_weights};
use super::lattice::{EvalSizes, Reader, Sentence};
use super::tokenizer::Tokenizer;
use super::{Config, Existing, LineEnding, format_g, open_lines};
use crate::tool_error::learn_error;
use mecab_rs::__private::files::DICRC_FILE;
use mecab_rs::Error;

mod optimizer;

const MAX_THREADS: usize = 512;
const PROGRESS_INTERVAL: usize = 100;

#[derive(Clone, Debug)]
pub struct TrainOptions {
    pub dic_dir: PathBuf,
    pub old_model: Option<PathBuf>,
    pub cost: f64,
    pub freq: usize,
    pub eta: f64,
    pub threads: usize,
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
            crlf: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrainReport {
    pub sentences: usize,
}

struct TrainSettings {
    cost: f64,
    eta: f64,
    eval_size: usize,
    unk_eval_size: usize,
    threads: usize,
    freq: usize,
}

impl TrainSettings {
    fn read(config: &Config) -> Self {
        Self {
            cost: config.number("cost"),
            eta: config.number("eta"),
            eval_size: config.number("eval-size"),
            unk_eval_size: config.number("unk-eval-size"),
            threads: config.number("thread"),
            freq: config.number("freq"),
        }
    }

    fn sizes(&self) -> EvalSizes {
        EvalSizes {
            eval_size: self.eval_size,
            unk_eval_size: self.unk_eval_size,
        }
    }

    fn validate(&self, dic_dir: &Path) -> Result<(), Error> {
        self.check_cost(dic_dir)?;
        self.check_eta(dic_dir)?;
        self.check_counts(dic_dir)?;
        self.check_threads(dic_dir)
    }

    fn check_cost(&self, dic_dir: &Path) -> Result<(), Error> {
        if self.cost.is_nan() || self.cost <= 0.0 {
            return Err(learn_error(
                dic_dir,
                format!("cost parameter is out of range: {}", format_g(self.cost)),
            ));
        }
        Ok(())
    }

    fn check_eta(&self, dic_dir: &Path) -> Result<(), Error> {
        if self.eta.is_nan() || self.eta <= 0.0 {
            return Err(learn_error(
                dic_dir,
                format!("eta is out of range: {}", format_g(self.eta)),
            ));
        }
        Ok(())
    }

    fn check_counts(&self, dic_dir: &Path) -> Result<(), Error> {
        if self.eval_size == 0 {
            return Err(learn_error(dic_dir, "eval-size is out of range: 0"));
        }
        if self.unk_eval_size == 0 {
            return Err(learn_error(dic_dir, "unk-eval-size is out of range: 0"));
        }
        if self.freq == 0 {
            return Err(learn_error(
                dic_dir,
                format!("freq is out of range: {}", self.freq),
            ));
        }
        Ok(())
    }

    fn check_threads(&self, dic_dir: &Path) -> Result<(), Error> {
        if self.threads == 0 || self.threads > MAX_THREADS {
            return Err(learn_error(
                dic_dir,
                format!("# thread is invalid: {}", self.threads),
            ));
        }
        Ok(())
    }
}

fn load_config(options: &TrainOptions) -> Result<Config, Error> {
    let mut config = Config::default();
    config.set("cost", &options.cost.to_string(), Existing::Replace);
    config.set("eta", &options.eta.to_string(), Existing::Replace);
    config.set("freq", &options.freq.to_string(), Existing::Replace);
    config.set("thread", &options.threads.to_string(), Existing::Replace);
    config.load(&options.dic_dir.join(DICRC_FILE))?;
    Ok(config)
}

fn load_previous_model(
    options: &TrainOptions,
    index: &mut FeatureIndex,
    charset: &str,
    config: &mut Config,
    log: &mut dyn Write,
) -> Result<Vec<f64>, Error> {
    let Some(old_model) = &options.old_model else {
        return Ok(Vec::new());
    };
    let _ = writeln!(log, "Using previous model: {}", old_model.display());
    let _ = writeln!(log, "--cost --freq and --eta options are overwritten.");
    let (alpha, settings) = index.reopen(old_model, charset)?;
    for (key, value) in settings {
        config.set(&key, &value, Existing::Replace);
    }
    Ok(alpha)
}

fn log_virtual_feature(log: &mut dyn Write, feature: &[u8]) {
    let _ = writeln!(
        log,
        "adding virtual node: {}",
        String::from_utf8_lossy(feature)
    );
}

fn log_sentence_progress(log: &mut dyn Write, count: usize) {
    if count.is_multiple_of(PROGRESS_INTERVAL) {
        let _ = write!(log, "{count}... ");
    }
}

fn read_corpus(
    corpus: &Path,
    log: &mut dyn Write,
    tokenizer: &mut Tokenizer,
    features: &mut Features,
    index: &mut FeatureIndex,
    sizes: EvalSizes,
) -> Result<(Vec<Sentence>, Vec<f64>), Error> {
    let _ = write!(log, "reading corpus ...");
    let mut lines = open_lines(corpus)?;
    let mut observed = Vec::new();
    let mut sentences = Vec::new();
    let mut reader = Reader {
        tokenizer,
        features,
        index,
        eval_size: sizes.eval_size,
        unk_eval_size: sizes.unk_eval_size,
        found: Vec::new(),
    };
    loop {
        let read = Sentence::read(&mut reader, &mut lines, &mut observed)?;
        let done = read.is_none();
        if let Some(read) = read {
            for feature in &read.virtual_features {
                log_virtual_feature(log, feature);
            }
            sentences.push(read.sentence);
        }
        log_sentence_progress(log, sentences.len());
        if done {
            break;
        }
    }
    Ok((sentences, observed))
}

fn log_header(
    log: &mut dyn Write,
    settings: &TrainSettings,
    sentence_count: usize,
    size: usize,
    charset: &str,
) {
    let TrainSettings {
        cost,
        eta,
        eval_size,
        unk_eval_size,
        threads,
        freq,
    } = *settings;
    let _ = writeln!(log);
    let _ = writeln!(log, "Number of sentences: {sentence_count}");
    let _ = writeln!(log, "Number of features:  {size}");
    let _ = writeln!(log, "eta:                 {eta:.5}");
    let _ = writeln!(log, "freq:                {freq}");
    let _ = writeln!(log, "eval-size:           {eval_size}");
    let _ = writeln!(log, "unk-eval-size:       {unk_eval_size}");
    let _ = writeln!(log, "threads:             {threads}");
    let _ = writeln!(log, "charset:             {charset}");
    let _ = writeln!(log, "C(sigma^2):          {cost:.5}\n");
}

fn model_header(settings: &TrainSettings, charset: &str) -> String {
    let TrainSettings {
        cost,
        eta,
        eval_size,
        unk_eval_size,
        freq,
        ..
    } = *settings;
    format!(
        "eta: {}\nfreq: {freq}\nC: {}\neval-size: {eval_size}\nunk-eval-size: {unk_eval_size}\ncharset: {charset}\n",
        format_g(eta),
        format_g(cost),
    )
}

pub fn train(
    options: &TrainOptions,
    corpus: &Path,
    model: &Path,
    log: &mut dyn Write,
) -> Result<TrainReport, Error> {
    let dic_dir = options.dic_dir.as_path();
    let mut config = load_config(options)?;
    let mut tokenizer = Tokenizer::open(dic_dir, &config)?;
    let mut features = Features::open(dic_dir)?;
    let mut index = FeatureIndex::open(dic_dir)?;
    let old_alpha = load_previous_model(options, &mut index, &tokenizer.charset, &mut config, log)?;

    let settings = TrainSettings::read(&config);
    settings.validate(dic_dir)?;

    let (sentences, mut observed) = read_corpus(
        corpus,
        log,
        &mut tokenizer,
        &mut features,
        &mut index,
        settings.sizes(),
    )?;
    if sentences.is_empty() {
        return Err(learn_error(corpus, "the corpus has no sentences"));
    }

    let renumbered = index.shrink(settings.freq, &mut observed);
    index.clear_cache();
    features.clear_rewrites();

    let size = index.size();
    observed.resize(size, 0.0);
    let old_alpha = remap_weights(&old_alpha, &renumbered, size);
    log_header(log, &settings, sentences.len(), size, &tokenizer.charset);

    let mut optimizer = Optimizer::new(&settings, sentences, size, observed, old_alpha);
    optimizer.run(dic_dir, index.pool(), &features, log)?;

    let _ = writeln!(log, "\nDone! writing model file ... ");
    index.save(
        model,
        &model_header(&settings, &tokenizer.charset),
        &optimizer.alpha,
        LineEnding::from(options.crlf),
    )?;

    Ok(TrainReport {
        sentences: optimizer.sentence_count,
    })
}
