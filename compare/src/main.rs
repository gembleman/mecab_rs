mod acc;
mod engines;
mod lines;
mod measure;
mod memory;
mod options;

use std::fs;
use std::time::Instant;

use engines::{Lindera, MecabRs, SDict, Sudachi, Vibrato};
use lines::Lines;
use measure::report;
use memory::memory;
use options::parse_args;

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
            d.set_max_grouping_size(
                std::num::NonZeroUsize::new(o.max_grouping).expect("--max-grouping is positive"),
            );
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
