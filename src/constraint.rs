use crate::feature_match::partial_match;

pub(crate) const NO_FEATURE: u32 = u32::MAX;
const ANY_BOUNDARY: u8 = 0;
const TOKEN_BOUNDARY: u8 = 1;
const INSIDE_TOKEN: u8 = 2;
const POSITION_SLACK: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryKind {
    Token,
    Inside,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Constraint<'a> {
    Boundary { position: usize, kind: BoundaryKind },
    Feature { start: usize, end: usize, pattern: &'a str },
}

#[derive(Default)]
pub(crate) struct ConstraintSet {
    boundary: Vec<u8>,
    feature_at: Vec<u32>,
    spans: Vec<(usize, usize)>,
    bytes: Vec<u8>,
}

impl ConstraintSet {
    pub(crate) fn load(&mut self, text_len: usize, constraints: &[Constraint<'_>]) {
        self.boundary.clear();
        self.feature_at.clear();
        self.spans.clear();
        self.bytes.clear();
        for constraint in constraints {
            match *constraint {
                Constraint::Boundary { position, kind } => {
                    if position != 0 && position < text_len {
                        let raw = match kind {
                            BoundaryKind::Token => TOKEN_BOUNDARY,
                            BoundaryKind::Inside => INSIDE_TOKEN,
                        };
                        self.set_boundary(text_len, position, raw);
                    }
                }
                Constraint::Feature { start, end, pattern } => {
                    if start >= end || end > text_len || pattern.contains('\0') {
                        continue;
                    }
                    self.set_feature(text_len, start, end, pattern.as_bytes());
                }
            }
        }
        if self.is_active() {
            self.set_boundary(text_len, 0, TOKEN_BOUNDARY);
            self.set_boundary(text_len, text_len, TOKEN_BOUNDARY);
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.boundary.is_empty()
    }

    pub(crate) fn is_token_boundary(&self, position: usize) -> bool {
        self.boundary_at(position) == TOKEN_BOUNDARY
    }

    pub(crate) fn is_inside_token(&self, position: usize) -> bool {
        self.boundary_at(position) == INSIDE_TOKEN
    }

    pub(crate) fn feature_at(&self, begin: usize) -> u32 {
        self.feature_at.get(begin).copied().unwrap_or(NO_FEATURE)
    }

    pub(crate) fn feature_bytes(&self, index: u32) -> &[u8] {
        let (start, end) = self.spans[index as usize];
        &self.bytes[start..end]
    }

    pub(crate) fn admits(&self, begin: usize, end: usize, feature: &[u8]) -> bool {
        if self.is_inside_token(end) {
            return false;
        }
        let index = self.feature_at(begin);
        if index == NO_FEATURE {
            return true;
        }
        self.is_token_boundary(begin)
            && self.is_token_boundary(end)
            && partial_match(self.feature_bytes(index), feature)
    }

    fn boundary_at(&self, position: usize) -> u8 {
        self.boundary.get(position).copied().unwrap_or(ANY_BOUNDARY)
    }

    fn set_boundary(&mut self, text_len: usize, position: usize, kind: u8) {
        if self.boundary.is_empty() {
            self.boundary.resize(text_len + POSITION_SLACK, ANY_BOUNDARY);
        }
        self.boundary[position] = kind;
    }

    fn set_feature(&mut self, text_len: usize, begin: usize, end: usize, feature: &[u8]) {
        if self.feature_at.is_empty() {
            self.feature_at.resize(text_len + POSITION_SLACK, NO_FEATURE);
        }
        let end = end.min(text_len);
        self.set_boundary(text_len, begin, TOKEN_BOUNDARY);
        self.set_boundary(text_len, end, TOKEN_BOUNDARY);
        for inside in begin + 1..end {
            self.set_boundary(text_len, inside, INSIDE_TOKEN);
        }
        let start = self.bytes.len();
        self.bytes.extend_from_slice(feature);
        self.spans.push((start, self.bytes.len()));
        self.feature_at[begin] = (self.spans.len() - 1) as u32;
    }
}
