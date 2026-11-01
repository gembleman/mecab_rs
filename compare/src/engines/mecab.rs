use super::Engine;
use crate::acc::Acc;

pub struct MecabRs<'d> {
    pub tagger: mecab_rs::Tagger<&'d mecab_rs::Dictionary>,
    pub feature_buffer: String,
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
