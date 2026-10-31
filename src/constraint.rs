use crate::error::Error;
use crate::feature_match::partial_match;
use crate::lattice::{RETAINED_MIN_ENTRIES, shrink_over};

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

#[derive(Clone, Debug)]
pub struct Constraints<'t> {
    text: &'t str,
    items: Vec<Constraint<'t>>,
}

impl<'t> Constraints<'t> {
    pub fn new(text: &'t str) -> Self {
        Self {
            text,
            items: Vec::new(),
        }
    }

    pub fn text(&self) -> &'t str {
        self.text
    }

    pub fn as_slice(&self) -> &[Constraint<'t>] {
        &self.items
    }

    pub fn boundary(&mut self, position: usize, kind: BoundaryKind) -> Result<&mut Self, Error> {
        if self.is_interior(position) {
            self.check_character_boundary(position)?;
            self.push_boundary(position, kind);
        }
        Ok(self)
    }

    pub fn feature(
        &mut self,
        start: usize,
        end: usize,
        pattern: &'t str,
    ) -> Result<&mut Self, Error> {
        if self.is_effective_feature(start, end, pattern) {
            self.check_character_boundary(start)?;
            self.check_character_boundary(end.min(self.text.len()))?;
            self.push_feature(start, end, pattern);
        }
        Ok(self)
    }

    pub(crate) fn push_boundary(&mut self, position: usize, kind: BoundaryKind) {
        if self.is_interior(position) {
            debug_assert!(self.text.is_char_boundary(position));
            self.items.push(Constraint::Boundary { position, kind });
        }
    }

    pub(crate) fn push_feature(&mut self, start: usize, end: usize, pattern: &'t str) {
        if self.is_effective_feature(start, end, pattern) {
            let end = end.min(self.text.len());
            debug_assert!(self.text.is_char_boundary(start) && self.text.is_char_boundary(end));
            self.items.push(Constraint::Feature {
                start,
                end,
                pattern,
            });
        }
    }

    fn is_interior(&self, position: usize) -> bool {
        position != 0 && position < self.text.len()
    }

    fn is_effective_feature(&self, start: usize, end: usize, pattern: &str) -> bool {
        start < end.min(self.text.len()) && !pattern.contains('\0')
    }

    fn check_character_boundary(&self, position: usize) -> Result<(), Error> {
        if self.text.is_char_boundary(position) {
            Ok(())
        } else {
            Err(Error::PositionInsideCharacter { position })
        }
    }
}

#[derive(Default)]
pub(crate) struct ConstraintSet {
    boundary: Vec<Boundary>,
    next_boundary: Vec<u32>,
    feature_at: Vec<Option<u32>>,
    spans: Vec<(usize, usize)>,
    bytes: String,
    scratch: String,
}

impl ConstraintSet {
    pub(crate) fn load(&mut self, text_len: usize, constraints: &[Constraint<'_>]) {
        self.boundary.clear();
        self.next_boundary.clear();
        self.feature_at.clear();
        self.shrink_oversized(text_len);
        self.spans.clear();
        self.bytes.clear();
        self.shrink_feature_storage(constraints);
        for constraint in constraints {
            match *constraint {
                Constraint::Boundary { position, kind } => {
                    debug_assert!(position != 0 && position < text_len);
                    let boundary = match kind {
                        BoundaryKind::Token => Boundary::Token,
                        BoundaryKind::Inside => Boundary::Inside,
                    };
                    self.set_boundary(text_len, position, boundary);
                }
                Constraint::Feature {
                    start,
                    end,
                    pattern,
                } => {
                    debug_assert!(start < end && end <= text_len);
                    self.set_feature(text_len, start, end, pattern);
                }
            }
        }
        if self.is_active() {
            self.set_boundary(text_len, 0, Boundary::Token);
            self.set_boundary(text_len, text_len, Boundary::Token);
            self.fill_next_boundary(text_len);
        }
    }

    pub(crate) fn next_token_boundary(&self, begin: usize) -> usize {
        self.next_boundary[begin] as usize
    }

    fn fill_next_boundary(&mut self, text_len: usize) {
        debug_assert!(u32::try_from(text_len).is_ok());
        self.next_boundary.resize(text_len + 1, 0);
        let mut next = text_len as u32;
        for position in (0..=text_len).rev() {
            self.next_boundary[position] = next;
            if self.boundary[position] == Boundary::Token {
                next = position as u32;
            }
        }
    }

    fn shrink_oversized(&mut self, text_len: usize) {
        let keep = RETAINED_MIN_ENTRIES.max(text_len.saturating_add(1));
        shrink_over(&mut self.boundary, keep);
        shrink_over(&mut self.next_boundary, keep);
        shrink_over(&mut self.feature_at, keep);
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

    fn shrink_feature_storage(&mut self, constraints: &[Constraint<'_>]) {
        let (count, byte_total) = constraints.iter().fold(
            (0usize, 0usize),
            |(count, bytes), constraint| match *constraint {
                Constraint::Feature { pattern, .. } => {
                    (count + 1, bytes.saturating_add(pattern.len()))
                }
                Constraint::Boundary { .. } => (count, bytes),
            },
        );
        shrink_over(&mut self.spans, RETAINED_MIN_ENTRIES.max(count));
        let keep_bytes = RETAINED_MIN_ENTRIES.max(byte_total);
        if self.bytes.capacity() > keep_bytes {
            self.bytes.shrink_to(keep_bytes);
        }
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
