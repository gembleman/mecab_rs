#[derive(Default, Clone, Copy)]
pub struct Acc {
    pub tokens: u64,
    pub surface_bytes: u64,
    pub feature_bytes: u64,
}

impl Acc {
    pub fn add(&mut self, o: Acc) {
        self.tokens += o.tokens;
        self.surface_bytes += o.surface_bytes;
        self.feature_bytes += o.feature_bytes;
    }
}
