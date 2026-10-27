use crate::constraint::{BoundaryKind, Constraint};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialInput<'a> {
    sentence: String,
    constraints: Vec<Constraint<'a>>,
}

const END_OF_SENTENCE_LINE: &str = "EOS";

fn split_line(line: &str) -> Option<(&str, Option<&str>)> {
    match line.split_once('\t') {
        Some((surface, rest)) => {
            let feature_pattern = rest.split('\t').next().unwrap_or(rest);
            Some((surface, Some(feature_pattern)))
        }
        None if line == END_OF_SENTENCE_LINE => None,
        None => Some((line, None)),
    }
}

impl<'a> PartialInput<'a> {
    pub fn parse(input: &'a str) -> Self {
        let input = input.split('\0').next().unwrap_or_default();
        let mut sentence = String::with_capacity(input.len());
        let mut constraints = Vec::new();
        for line in input.split('\n') {
            let Some((surface, feature)) = split_line(line) else {
                break;
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
