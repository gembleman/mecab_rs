use super::Engine;
use crate::acc::Acc;

pub type SDict = sudachi::dic::dictionary::JapaneseDictionary;

pub struct Sudachi<'d> {
    pub tokenizer: sudachi::analysis::stateful_tokenizer::StatefulTokenizer<&'d SDict>,
    pub morphemes: sudachi::prelude::MorphemeList<&'d SDict>,
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
