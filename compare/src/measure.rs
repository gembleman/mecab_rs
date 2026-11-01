use std::fs;
use std::hint::black_box;
use std::time::Instant;

use crate::acc::Acc;
use crate::engines::Engine;
use crate::lines::Lines;
use crate::memory::{Mem, mem_json, memory};
use crate::options::Opts;

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
pub fn report(
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
