use super::Engine;
use crate::acc::Acc;

pub struct Lindera {
    pub worker: lindera::worker::SegmentWorker,
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
