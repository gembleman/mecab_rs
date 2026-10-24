use crate::constraint::{BoundaryKind, Constraint};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialInput<'a> {
    sentence: String,
    constraints: Vec<Constraint<'a>>,
}

impl<'a> PartialInput<'a> {
    pub fn parse(input: &'a str) -> Self {
        let input = input.split('\0').next().unwrap_or_default();
        let mut sentence = String::with_capacity(input.len());
        let mut constraints = Vec::new();
        for line in input.split('\n') {
            let (surface, feature) = match line.split_once('\t') {
                Some((surface, rest)) => (surface, Some(rest.split('\t').next().unwrap_or(rest))),
                None if line == "EOS" => break,
                None => (line, None),
            };
            let start = sentence.len();
            let end = start + surface.len();
            sentence.push_str(surface);
            constraints.push(Constraint::Boundary {
                position: start,
                kind: BoundaryKind::Token,
            });
            constraints.push(Constraint::Boundary {
                position: end,
                kind: BoundaryKind::Token,
            });
            if let Some(pattern) = feature {
                constraints.push(Constraint::Feature {
                    start,
                    end,
                    pattern,
                });
            }
        }
        Self {
            sentence,
            constraints,
        }
    }

    pub fn sentence(&self) -> &str {
        &self.sentence
    }

    pub fn constraints(&self) -> &[Constraint<'a>] {
        &self.constraints
    }

    pub fn into_parts(self) -> (String, Vec<Constraint<'a>>) {
        (self.sentence, self.constraints)
    }
}
