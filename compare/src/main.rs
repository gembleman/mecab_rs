use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Default, Clone, Copy)]
struct Acc {
    tokens: u64,
    surface_bytes: u64,
    feature_bytes: u64,
}

impl Acc {
    fn add(&mut self, o: Acc) {
        self.tokens += o.tokens;
        self.surface_bytes += o.surface_bytes;
        self.feature_bytes += o.feature_bytes;
    }
}

struct Lines<'a> {
    text: &'a str,
    ends: Vec<u32>,
    bytes: usize,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        assert!(u32::try_from(text.len()).is_ok());
        let count = text.bytes().filter(|&byte| byte == b'\n').count() + 1;
        let mut ends = Vec::with_capacity(count);
        let base = text.as_ptr() as usize;
        ends.extend(
            text.lines()
                .map(|line| (line.as_ptr() as usize - base + line.len()) as u32),
        );
        let mut lines = Self {
            text,
            ends,
            bytes: 0,
        };
        lines.bytes = lines.iter().map(str::len).sum();
        lines
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    fn truncate(&mut self, count: usize) {
        self.ends.truncate(count);
        self.bytes = self.iter().map(str::len).sum();
    }

    fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        let text = self.text;
        let mut start = 0usize;
        self.ends.iter().map(move |&end| {
            let end = end as usize;
            debug_assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
            let line = unsafe { text.get_unchecked(start..end) };
            let terminator = if text.as_bytes().get(end) == Some(&b'\r') {
                2
            } else {
                1
            };
            start = end + terminator;
            line
        })
    }
}

trait Engine {
    fn surface(&mut self, line: &str) -> Acc;
    fn feature(&mut self, line: &str) -> Acc;
    fn dump(&mut self, line: &str, out: &mut String);
}

struct MecabRs<'d> {
    tagger: mecab_rs::Tagger<&'d mecab_rs::Dictionary>,
    feature_buffer: String,
}

fn is_word(k: mecab_rs::NodeKind) -> bool {
    matches!(k, mecab_rs::NodeKind::Normal | mecab_rs::NodeKind::Unknown)
}

impl Engine for MecabRs<'_> {
    fn surface(&mut self, line: &str) -> Acc {
        let mut a = Acc::default();
        for n in self.tagger.parse(line).expect("parse") {
            if is_word(n.kind()) {
                a.tokens += 1;
                a.surface_bytes += n.surface().len() as u64;
            }
        }
        a
    }
    fn feature(&mut self, line: &str) -> Acc {
        let mut a = Acc::default();
        for n in self.tagger.parse(line).expect("parse") {
            if is_word(n.kind()) {
                a.tokens += 1;
                a.surface_bytes += n.surface().len() as u64;
                self.feature_buffer.clear();
                n.feature().write_to(&mut self.feature_buffer);
                a.feature_bytes += self.feature_buffer.len() as u64;
            }
        }
        a
    }
    fn dump(&mut self, line: &str, out: &mut String) {
        for n in self.tagger.parse(line).expect("parse") {
            if is_word(n.kind()) {
                out.push_str(n.surface());
                out.push('\u{1f}');
            }
        }
    }
}

struct Vibrato<'t> {
    worker: vibrato::tokenizer::worker::Worker<'t>,
}

impl Engine for Vibrato<'_> {
    fn surface(&mut self, line: &str) -> Acc {
        let mut a = Acc::default();
        self.worker.reset_sentence(line);
        self.worker.tokenize();
        for i in 0..self.worker.num_tokens() {
            a.tokens += 1;
            a.surface_bytes += self.worker.token(i).surface().len() as u64;
        }
        a
    }
    fn feature(&mut self, line: &str) -> Acc {
        let mut a = Acc::default();
        self.worker.reset_sentence(line);
        self.worker.tokenize();
        for i in 0..self.worker.num_tokens() {
            let t = self.worker.token(i);
            a.tokens += 1;
            a.surface_bytes += t.surface().len() as u64;
            a.feature_bytes += t.feature().len() as u64;
        }
        a
    }
    fn dump(&mut self, line: &str, out: &mut String) {
        self.worker.reset_sentence(line);
        self.worker.tokenize();
        for i in 0..self.worker.num_tokens() {
            out.push_str(self.worker.token(i).surface());
            out.push('\u{1f}');
        }
    }
}

struct Lindera {
    worker: lindera::worker::SegmentWorker,
}

impl Engine for Lindera {
    fn surface(&mut self, line: &str) -> Acc {
        let mut a = Acc::default();
        for t in self.worker.segment(line).expect("segment") {
            a.tokens += 1;
            a.surface_bytes += t.surface.len() as u64;
        }
        a
    }
    fn feature(&mut self, line: &str) -> Acc {
        let mut a = Acc::default();
        for mut t in self.worker.segment(line).expect("segment") {
            a.tokens += 1;
            a.surface_bytes += t.surface.len() as u64;
            a.feature_bytes += t.details_iter().map(|d| d.len() as u64).sum::<u64>();
        }
        a
    }
    fn dump(&mut self, line: &str, out: &mut String) {
        for t in self.worker.segment(line).expect("segment") {
            out.push_str(&t.surface);
            out.push('\u{1f}');
        }
    }
}

type SDict = sudachi::dic::dictionary::JapaneseDictionary;

struct Sudachi<'d> {
    tokenizer: sudachi::analysis::stateful_tokenizer::StatefulTokenizer<&'d SDict>,
    morphemes: sudachi::prelude::MorphemeList<&'d SDict>,
}

impl Sudachi<'_> {
    fn run(&mut self, line: &str) {
        self.tokenizer.reset().push_str(line);
        self.tokenizer.do_tokenize().expect("tokenize");
        self.morphemes
            .collect_results(&mut self.tokenizer)
            .expect("collect");
    }
}

impl Engine for Sudachi<'_> {
    fn surface(&mut self, line: &str) -> Acc {
        self.run(line);
        let mut a = Acc::default();
        for m in self.morphemes.iter() {
            a.tokens += 1;
            a.surface_bytes += m.surface().len() as u64;
        }
        a
    }
    fn feature(&mut self, line: &str) -> Acc {
        self.run(line);
        let mut a = Acc::default();
        for m in self.morphemes.iter() {
            a.tokens += 1;
            a.surface_bytes += m.surface().len() as u64;
            a.feature_bytes += m
                .part_of_speech()
                .iter()
                .map(|s| s.len() as u64)
                .sum::<u64>()
                + m.reading_form().len() as u64
                + m.dictionary_form().len() as u64;
        }
        a
    }
    fn dump(&mut self, line: &str, out: &mut String) {
        self.run(line);
        for m in self.morphemes.iter() {
            out.push_str(&m.surface());
            out.push('\u{1f}');
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Mem {
    ws: usize,
    peak_ws: usize,
    private: usize,
    peak_private: usize,
}

#[cfg(windows)]
fn memory() -> Mem {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_ws: usize,
        ws: usize,
        quota: [usize; 4],
        pagefile: usize,
        peak_pagefile: usize,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters {
        cb: size_of::<Counters>() as u32,
        ..Default::default()
    };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    assert!(ok != 0, "K32GetProcessMemoryInfo failed");
    Mem {
        ws: c.ws,
        peak_ws: c.peak_ws,
        private: c.pagefile,
        peak_private: c.peak_pagefile,
    }
}

#[cfg(not(windows))]
fn memory() -> Mem {
    Mem::default()
}

fn mem_json(base: Mem, loaded: Mem, end: Mem) -> String {
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    format!(
        "{{\"base_ws_mb\":{:.2},\"base_private_mb\":{:.2},\"loaded_ws_mb\":{:.2},\"loaded_private_mb\":{:.2},\"peak_ws_mb\":{:.2},\"peak_private_mb\":{:.2}}}",
        mb(base.ws),
        mb(base.private),
        mb(loaded.ws),
        mb(loaded.private),
        mb(end.peak_ws),
        mb(end.peak_private)
    )
}

struct Opts {
    tool: String,
    dict: PathBuf,
    corpus: PathBuf,
    warmup: usize,
    passes: usize,
    limit: Option<usize>,
    dump: Option<PathBuf>,
    max_grouping: usize,
}

fn parse_args() -> Opts {
    let mut tool = None;
    let mut dict = None;
    let mut corpus = None;
    let (mut warmup, mut passes, mut limit, mut dump) = (2, 7, None, None);
    let mut max_grouping = 24;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().unwrap_or_else(|| panic!("{k} needs a value"));
        match k.as_str() {
            "--tool" => tool = Some(v()),
            "--dict" => dict = Some(PathBuf::from(v())),
            "--corpus" => corpus = Some(PathBuf::from(v())),
            "--warmup" => warmup = v().parse().unwrap(),
            "--passes" => passes = v().parse().unwrap(),
            "--limit" => limit = Some(v().parse().unwrap()),
            "--dump" => dump = Some(PathBuf::from(v())),
            "--max-grouping" => max_grouping = v().parse().unwrap(),
            _ => panic!("unknown option {k}"),
        }
    }
    Opts {
        tool: tool.expect("--tool"),
        dict: dict.expect("--dict"),
        corpus: corpus.expect("--corpus"),
        warmup,
        passes,
        limit,
        dump,
        max_grouping,
    }
}

fn measure(
    engine: &mut dyn Engine,
    lines: &Lines,
    f: fn(&mut dyn Engine, &str) -> Acc,
    warmup: usize,
    passes: usize,
) -> (Vec<f64>, Acc) {
    let mut acc = Acc::default();
    for _ in 0..warmup {
        for l in lines.iter() {
            acc.add(f(engine, black_box(l)));
        }
    }
    let mut times = Vec::new();
    let mut last = Acc::default();
    for _ in 0..passes {
        let mut a = Acc::default();
        let t = Instant::now();
        for l in lines.iter() {
            a.add(f(engine, black_box(l)));
        }
        times.push(t.elapsed().as_secs_f64());
        last = a;
    }
    black_box(acc);
    (times, last)
}

fn stats_json(times: &[f64], acc: Acc, n_lines: usize, n_bytes: usize) -> String {
    let mut s = times.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = s[0];
    let median = s[s.len() / 2];
    let r = |t: f64| (n_lines as f64 / t, n_bytes as f64 / t / 1e6);
    let (mps, mmb) = r(median);
    let (bps, bmb) = r(min);
    format!(
        "{{\"median_s\":{median:.6},\"min_s\":{min:.6},\"median_sent_per_s\":{mps:.1},\"median_mb_per_s\":{mmb:.4},\"best_sent_per_s\":{bps:.1},\"best_mb_per_s\":{bmb:.4},\"tokens\":{},\"surface_bytes\":{},\"feature_bytes\":{}}}",
        acc.tokens, acc.surface_bytes, acc.feature_bytes
    )
}

#[allow(clippy::too_many_arguments)]
fn report(
    o: &Opts,
    base: Mem,
    version: &str,
    load_s: f64,
    lines: &Lines,
    n_bytes: usize,
    es: &mut dyn Engine,
    ef: &mut dyn Engine,
) {
    let loaded = memory();
    let (ts, as_) = measure(es, lines, |e, l| e.surface(l), o.warmup, o.passes);
    let (tf, af) = measure(ef, lines, |e, l| e.feature(l), o.warmup, o.passes);
    let end = memory();
    if let Some(p) = &o.dump {
        let mut out = String::new();
        for l in lines.iter() {
            ef.dump(l, &mut out);
            out.push('\n');
        }
        fs::write(p, out).expect("write dump");
    }
    println!(
        "{{\"tool\":\"{}\",\"version\":\"{}\",\"load_s\":{load_s:.4},\"sentences\":{},\"corpus_bytes\":{n_bytes},\"warmup\":{},\"passes\":{},\"surface\":{},\"feature\":{},\"mem\":{}}}",
        o.tool,
        version,
        lines.len(),
        o.warmup,
        o.passes,
        stats_json(&ts, as_, lines.len(), n_bytes),
        stats_json(&tf, af, lines.len(), n_bytes),
        mem_json(base, loaded, end),
    );
}

fn main() {
    let o = parse_args();
    let text = fs::read_to_string(&o.corpus).expect("read corpus");
    let mut lines = Lines::new(&text);
    if let Some(n) = o.limit {
        lines.truncate(n);
    }
    let n_bytes = lines.bytes;
    let base = memory();

    match o.tool.as_str() {
        "mecab_rs" => {
            let t = Instant::now();
            let mut d = mecab_rs::Dictionary::open(&o.dict).expect("open mecab_rs dictionary");
            d.set_max_grouping_size(o.max_grouping);
            let load_s = t.elapsed().as_secs_f64();
            let mut es = MecabRs {
                tagger: mecab_rs::Tagger::new(&d),
                feature_buffer: String::new(),
            };
            let mut ef = MecabRs {
                tagger: mecab_rs::Tagger::new(&d),
                feature_buffer: String::new(),
            };
            report(
                &o,
                base,
                "mecab_rs 0.1.0",
                load_s,
                &lines,
                n_bytes,
                &mut es,
                &mut ef,
            );
        }
        "vibrato" => {
            let t = Instant::now();
            let r = zstd::Decoder::new(fs::File::open(&o.dict).expect("open")).expect("zstd");
            let dict = vibrato::Dictionary::read(r).expect("read vibrato dictionary");
            let tk = vibrato::Tokenizer::new(dict)
                .ignore_space(true)
                .expect("ignore_space")
                .max_grouping_len(o.max_grouping);
            let load_s = t.elapsed().as_secs_f64();
            let mut es = Vibrato {
                worker: tk.new_worker(),
            };
            let mut ef = Vibrato {
                worker: tk.new_worker(),
            };
            report(
                &o,
                base,
                "vibrato 0.5.2",
                load_s,
                &lines,
                n_bytes,
                &mut es,
                &mut ef,
            );
        }
        "lindera" => {
            let t = Instant::now();
            let dict = lindera::dictionary::load_dictionary(o.dict.to_str().unwrap())
                .expect("load lindera dictionary");
            let seg = lindera::segmenter::Segmenter::new(lindera::mode::Mode::Normal, dict, None)
                .max_grouping_len(Some(o.max_grouping));
            let load_s = t.elapsed().as_secs_f64();
            let mut es = Lindera {
                worker: seg.new_worker(),
            };
            let mut ef = Lindera {
                worker: seg.new_worker(),
            };
            report(
                &o,
                base,
                "lindera 7.0.0",
                load_s,
                &lines,
                n_bytes,
                &mut es,
                &mut ef,
            );
        }
        "sudachi" => {
            use sudachi::analysis::stateful_tokenizer::StatefulTokenizer;
            use sudachi::prelude::MorphemeList;
            let t = Instant::now();
            let cfg = sudachi::config::Config::new(None, None, Some(o.dict.clone()))
                .expect("sudachi config");
            let d = SDict::from_cfg(&cfg).expect("load sudachi dictionary");
            let load_s = t.elapsed().as_secs_f64();
            let ts = StatefulTokenizer::create(&d, false, sudachi::analysis::Mode::C);
            let mut es = Sudachi {
                tokenizer: ts,
                morphemes: MorphemeList::empty(&d),
            };
            let tf = StatefulTokenizer::create(&d, false, sudachi::analysis::Mode::C);
            let mut ef = Sudachi {
                tokenizer: tf,
                morphemes: MorphemeList::empty(&d),
            };
            report(
                &o,
                base,
                "sudachi.rs 0.7.1-a1 (mode C)",
                load_s,
                &lines,
                n_bytes,
                &mut es,
                &mut ef,
            );
        }
        other => panic!("unknown tool {other}"),
    }
}
