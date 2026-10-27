use crate::feature_match::partial_match;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum Boundary {
    #[default]
    Any,
    Token,
    Inside,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryKind {
    Token,
    Inside,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Constraint<'a> {
    Boundary {
        position: usize,
        kind: BoundaryKind,
    },
    Feature {
        start: usize,
        end: usize,
        pattern: &'a str,
    },
}

#[derive(Default)]
pub(crate) struct ConstraintSet {
    boundary: Vec<Boundary>,
    feature_at: Vec<Option<u32>>,
    spans: Vec<(usize, usize)>,
    bytes: String,
    scratch: String,
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
                        let boundary = match kind {
                            BoundaryKind::Token => Boundary::Token,
                            BoundaryKind::Inside => Boundary::Inside,
                        };
                        self.set_boundary(text_len, position, boundary);
                    }
                }
                Constraint::Feature {
                    start,
                    end,
                    pattern,
                } => {
                    if start >= end || end > text_len || pattern.contains('\0') {
                        continue;
                    }
                    self.set_feature(text_len, start, end, pattern);
                }
            }
        }
        if self.is_active() {
            self.set_boundary(text_len, 0, Boundary::Token);
            self.set_boundary(text_len, text_len, Boundary::Token);
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.boundary.is_empty()
    }

    pub(crate) fn is_token_boundary(&self, position: usize) -> bool {
        self.boundary_at(position) == Boundary::Token
    }

    pub(crate) fn is_inside_token(&self, position: usize) -> bool {
        self.boundary_at(position) == Boundary::Inside
    }

    pub(crate) fn feature_at(&self, begin: usize) -> Option<u32> {
        self.feature_at.get(begin).copied().flatten()
    }

    pub(crate) fn feature_str(&self, index: u32) -> &str {
        let (start, end) = self.spans[index as usize];
        &self.bytes[start..end]
    }

    pub(crate) fn admits(
        &mut self,
        begin: usize,
        end: usize,
        feature: impl FnOnce(&mut String),
    ) -> bool {
        if self.is_inside_token(end) {
            return false;
        }
        let Some(index) = self.feature_at(begin) else {
            return true;
        };
        if !self.is_token_boundary(begin) || !self.is_token_boundary(end) {
            return false;
        }
        self.scratch.clear();
        feature(&mut self.scratch);
        partial_match(self.feature_str(index).as_bytes(), self.scratch.as_bytes())
    }

    fn boundary_at(&self, position: usize) -> Boundary {
        self.boundary.get(position).copied().unwrap_or_default()
    }

    fn set_boundary(&mut self, text_len: usize, position: usize, boundary: Boundary) {
        if self.boundary.is_empty() {
            self.boundary.resize(text_len + 1, Boundary::Any);
        }
        self.boundary[position] = boundary;
    }

    fn set_feature(&mut self, text_len: usize, begin: usize, end: usize, feature: &str) {
        if self.feature_at.is_empty() {
            self.feature_at.resize(text_len + 1, None);
        }
        self.set_boundary(text_len, begin, Boundary::Token);
        self.set_boundary(text_len, end, Boundary::Token);
        for inside in begin + 1..end {
            self.set_boundary(text_len, inside, Boundary::Inside);
        }
        let start = self.bytes.len();
        self.bytes.push_str(feature);
        self.spans.push((start, self.bytes.len()));
        self.feature_at[begin] = Some((self.spans.len() - 1) as u32);
    }
}
