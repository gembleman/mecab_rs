use super::Engine;
use crate::acc::Acc;

pub struct Vibrato<'t> {
    pub worker: vibrato::tokenizer::worker::Worker<'t>,
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
