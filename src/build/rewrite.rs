
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::text::{
    BUF_SIZE, Lines, atoi, cstr, escape_csv_element, failure, tokenize_csv, tokenize2,
};
use crate::error::Error;

struct RewritePattern {
    source: Vec<Vec<u8>>,
    destination: Vec<Vec<u8>>,
}

fn columns(text: &[u8], max: usize) -> Vec<Vec<u8>> {
    let mut buffer = text[..text.len().min(BUF_SIZE - 1)].to_vec();
    buffer.push(0);
    tokenize_csv(&mut buffer, max)
        .into_iter()
        .map(|start| cstr(&buffer, start).to_vec())
        .collect()
}

fn matches(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.first() == Some(&b'*') || pattern == text {
        return true;
    }
    if pattern.len() >= 3 && pattern[0] == b'(' && pattern[pattern.len() - 1] == b')' {
        return pattern[1..pattern.len() - 1]
            .split(|&byte| byte == b'|')
            .any(|alternative| alternative == text);
    }
    false
}

impl RewritePattern {
    fn new(source: &[u8], destination: &[u8]) -> Self {
        Self {
            source: columns(source, 512),
            destination: columns(destination, 512),
        }
    }

    fn rewrite(&self, input: &[&[u8]], output: &mut Vec<u8>) -> Result<bool, String> {
        if self.source.len() > input.len() {
            return Ok(false);
        }
        for (pattern, text) in self.source.iter().zip(input) {
            if !matches(pattern, text) {
                return Ok(false);
            }
        }
        output.clear();
        for (index, template) in self.destination.iter().enumerate() {
            let mut element = Vec::new();
            let mut at = 0;
            while at < template.len() {
                if template[at] == b'$' {
                    at += 1;
                    let mut number = 0usize;
                    while at < template.len() && template[at].is_ascii_digit() {
                        number = number
                            .wrapping_mul(10)
                            .wrapping_add(usize::from(template[at] - b'0'));
                        at += 1;
                    }
                    if number == 0 || number > input.len() {
                        return Err(format!(
                            " out of range: [{}] {number}",
                            String::from_utf8_lossy(template)
                        ));
                    }
                    element.extend_from_slice(input[number - 1]);
                    if at < template.len() {
                        element.push(template[at]);
                    }
                } else {
                    element.push(template[at]);
                }
                at += 1;
            }
            escape_csv_element(&mut element);
            output.extend_from_slice(&element);
            if index + 1 != self.destination.len() {
                output.push(b',');
            }
        }
        Ok(true)
    }
}

#[derive(Default)]
struct RewriteRules(Vec<RewritePattern>);

impl RewriteRules {
    fn rewrite(&self, input: &[&[u8]], output: &mut Vec<u8>) -> Result<bool, String> {
        for rule in &self.0 {
            if rule.rewrite(input, output)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn rule_of(line: &mut [u8], path: &Path) -> Result<RewritePattern, Error> {
    let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
    let found = tokenize2(line, b" \t", 3);
    if found.len() < 2 {
        return Err(failure(path, format!("format error: {shown}")));
    }
    let mut destination = cstr(line, found[1]).to_vec();
    if found.len() >= 3 {
        destination.push(b' ');
        destination.extend_from_slice(cstr(line, found[2]));
    }
    Ok(RewritePattern::new(cstr(line, found[0]), &destination))
}

fn with_columns<T>(
    feature: &[u8],
    path: &Path,
    use_columns: impl FnOnce(&[&[u8]]) -> Result<T, Error>,
) -> Result<T, Error> {
    if feature.len() >= BUF_SIZE - 1 {
        return Err(failure(path, "too long feature"));
    }
    let mut buffer = feature.to_vec();
    buffer.push(0);
    let found = tokenize_csv(&mut buffer, BUF_SIZE);
    let input: Vec<&[u8]> = found.iter().map(|&start| cstr(&buffer, start)).collect();
    use_columns(&input)
}

#[derive(Clone, Default)]
pub(crate) struct Rewritten {
    pub(crate) unigram: Vec<u8>,
    pub(crate) left: Vec<u8>,
    pub(crate) right: Vec<u8>,
}

pub(crate) struct Rewriter {
    unigram: RewriteRules,
    left: RewriteRules,
    right: RewriteRules,
    cache: HashMap<Vec<u8>, (bool, Rewritten)>,
}

impl Rewriter {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let mut lines = Lines::open(path, None)?;
        let mut rewriter = Self {
            unigram: RewriteRules::default(),
            left: RewriteRules::default(),
            right: RewriteRules::default(),
            cache: HashMap::new(),
        };
        let mut section = 0;
        while let Some(line) = lines.next_line()? {
            if line[0] == 0 || line[0] == b'#' {
                continue;
            }
            match cstr(line, 0) {
                b"[unigram rewrite]" => section = 1,
                b"[left rewrite]" => section = 2,
                b"[right rewrite]" => section = 3,
                _ => {
                    let rule = rule_of(line, path)?;
                    match section {
                        1 => rewriter.unigram.0.push(rule),
                        2 => rewriter.left.0.push(rule),
                        3 => rewriter.right.0.push(rule),
                        _ => return Err(failure(path, "no sections found")),
                    }
                }
            }
        }
        Ok(rewriter)
    }

    pub(crate) fn rewrite(&self, feature: &[u8], path: &Path) -> Result<(bool, Rewritten), Error> {
        with_columns(feature, path, |input| {
            let mut result = Rewritten::default();
            let fail = |reason: String| failure(path, reason);
            let matched = self
                .unigram
                .rewrite(input, &mut result.unigram)
                .map_err(fail)?
                && self.left.rewrite(input, &mut result.left).map_err(fail)?
                && self.right.rewrite(input, &mut result.right).map_err(fail)?;
            Ok((matched, result))
        })
    }

    pub(crate) fn rewrite_cached(
        &mut self,
        feature: &[u8],
        path: &Path,
    ) -> Result<(bool, Rewritten), Error> {
        if let Some(found) = self.cache.get(feature) {
            return Ok(found.clone());
        }
        let result = self.rewrite(feature, path)?;
        if result.0 {
            self.cache.insert(feature.to_vec(), result.clone());
        }
        Ok(result)
    }
}

pub(super) struct PosIds {
    rules: RewriteRules,
    path: PathBuf,
}

impl PosIds {
    pub(super) fn open(dic_dir: &Path) -> Result<Self, Error> {
        let path = dic_dir.join("pos-id.def");
        let mut rules = RewriteRules::default();
        if !path.is_file() {
            eprintln!("{} is not found. minimum setting is used", path.display());
            rules.0.push(RewritePattern::new(b"*", b"1"));
            return Ok(Self { rules, path });
        }
        let path = path.as_path();
        let mut lines = Lines::open(path, None)?;
        while let Some(line) = lines.next_line()? {
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            let found = tokenize2(line, b" \t", 2);
            if found.len() != 2 {
                return Err(failure(path, format!("format error: {shown}")));
            }
            let id = cstr(line, found[1]);
            if !id.iter().all(u8::is_ascii_digit) {
                return Err(failure(
                    path,
                    format!("not a number: {}", String::from_utf8_lossy(id)),
                ));
            }
            rules.0.push(RewritePattern::new(cstr(line, found[0]), id));
        }
        Ok(Self {
            rules,
            path: path.to_path_buf(),
        })
    }

    pub(super) fn id(&self, feature: &[u8]) -> Result<i32, Error> {
        with_columns(feature, &self.path, |input| {
            let mut output = Vec::new();
            if self
                .rules
                .rewrite(input, &mut output)
                .map_err(|reason| failure(&self.path, reason))?
            {
                Ok(atoi(&output))
            } else {
                Ok(-1)
            }
        })
    }
}

pub(super) struct ContextIds {
    left: HashMap<Vec<u8>, i32>,
    right: HashMap<Vec<u8>, i32>,
    left_path: std::path::PathBuf,
    right_path: std::path::PathBuf,
}

fn open_map(path: &Path) -> Result<HashMap<Vec<u8>, i32>, Error> {
    let mut lines = Lines::open(path, None)?;
    let mut map = HashMap::new();
    while let Some(line) = lines.next_line()? {
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let found = tokenize2(line, b" \t", 2);
        if found.len() != 2 {
            return Err(failure(path, format!("format error: {shown}")));
        }
        let id = atoi(cstr(line, found[0]));
        map.entry(cstr(line, found[1]).to_vec()).or_insert(id);
    }
    Ok(map)
}

impl ContextIds {
    pub(super) fn open(left: &Path, right: &Path) -> Result<Self, Error> {
        Ok(Self {
            left: open_map(left)?,
            right: open_map(right)?,
            left_path: left.to_path_buf(),
            right_path: right.to_path_buf(),
        })
    }

    pub(super) fn left_size(&self) -> usize {
        self.left.len()
    }

    pub(super) fn right_size(&self) -> usize {
        self.right.len()
    }

    pub(super) fn left_id(&self, feature: &[u8]) -> Result<i32, Error> {
        self.left.get(feature).copied().ok_or_else(|| {
            failure(
                &self.left_path,
                format!(
                    "cannot find LEFT-ID  for {}",
                    String::from_utf8_lossy(feature)
                ),
            )
        })
    }

    pub(super) fn right_id(&self, feature: &[u8]) -> Result<i32, Error> {
        self.right.get(feature).copied().ok_or_else(|| {
            failure(
                &self.right_path,
                format!(
                    "cannot find RIGHT-ID  for {}",
                    String::from_utf8_lossy(feature)
                ),
            )
        })
    }
}
