#[derive(Clone, Copy)]
pub(in crate::packed) struct Tokens<'k> {
    features: &'k [u32],
    key_ids: &'k [u32],
    keys: &'k [String],
    unknown: &'k [u32],
    start: usize,
    end: usize,
}

impl<'k> Tokens<'k> {
    pub(in crate::packed) fn new(
        features: &'k [u32],
        key_ids: &'k [u32],
        keys: &'k [String],
        unknown: &'k [u32],
    ) -> Self {
        debug_assert_eq!(features.len(), key_ids.len());
        Self {
            features,
            key_ids,
            keys,
            unknown,
            start: 0,
            end: key_ids.len() + unknown.len(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.end - self.start
    }

    pub(super) fn at(&self, index: usize) -> (u32, &'k str) {
        debug_assert!(index < self.len());
        let absolute = self.start + index;
        match self.key_ids.get(absolute) {
            Some(&key) => (self.features[absolute], self.keys[key as usize].as_str()),
            None => (self.unknown[absolute - self.key_ids.len()], ""),
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (u32, &'k str)> + '_ {
        (0..self.len()).map(|index| self.at(index))
    }

    pub(super) fn chunks(&self, chunk_len: usize) -> impl Iterator<Item = Tokens<'k>> + '_ {
        let chunk_len = chunk_len.max(1);
        (self.start..self.end)
            .step_by(chunk_len)
            .map(move |start| Tokens {
                start,
                end: self.end.min(start.saturating_add(chunk_len)),
                ..*self
            })
    }
}
