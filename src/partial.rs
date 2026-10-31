use crate::constraint::{BoundaryKind, Constraints};
use crate::error::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialInput<'a> {
    sentence: String,
    lines: Vec<Line<'a>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Line<'a> {
    start: usize,
    end: usize,
    feature: Option<&'a str>,
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
    pub fn parse(input: &'a str) -> Result<Self, Error> {
        if input.contains('\0') {
            return Err(Error::NulInText);
        }
        let mut sentence = String::with_capacity(input.len());
        let mut lines = Vec::new();
        for line in input.split('\n') {
            let Some((surface, feature)) = split_line(line) else {
                break;
            };
            let start = sentence.len();
            sentence.push_str(surface);
            lines.push(Line {
                start,
                end: sentence.len(),
                feature,
            });
        }
        Ok(Self { sentence, lines })
    }

    pub fn sentence(&self) -> &str {
        &self.sentence
    }

    pub fn constraints(&self) -> Constraints<'_> {
        let mut constraints = Constraints::new(&self.sentence);
        for line in &self.lines {
            constraints.push_boundary(line.start, BoundaryKind::Token);
            constraints.push_boundary(line.end, BoundaryKind::Token);
            if let Some(pattern) = line.feature {
                constraints.push_feature(line.start, line.end, pattern);
            }
        }
        constraints
    }
}
