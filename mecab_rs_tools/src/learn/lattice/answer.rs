use std::path::Path as FilePath;

use super::{NodeId, PathId, Reader, Sentence};
use crate::build::text::{Lines, cstr};
use crate::learn::features::{PoolOffset, ids_from};
use crate::learn::surface_and_feature;
use crate::learn::tokenizer::{EOS, UNKNOWN};
use crate::tool_error::{Position, learn_error};
use mecab_rs::Error;

const SENTENCE_TOO_LONG: &str = "a sentence is longer than 4294967295 bytes";

pub(super) struct Word {
    pub(super) surface: Vec<u8>,
    pub(super) feature: Vec<u8>,
}

pub(super) struct AnswerWords {
    pub(super) words: Vec<Word>,
    pub(super) text: Vec<u8>,
}

pub(super) fn read_words(lines: &mut Lines, path: &FilePath) -> Result<Option<AnswerWords>, Error> {
    let mut words: Vec<Word> = Vec::new();
    let mut text = Vec::new();
    loop {
        let Some((number, line)) = lines.next_line()? else {
            if words.is_empty() {
                return Ok(None);
            }
            return Err(learn_error(path, "missing EOS at the end of the corpus"));
        };
        let line = cstr(line, 0);
        if line == b"EOS" || line.is_empty() {
            break;
        }
        let at = Position::new(path, number);
        let Some((surface, feature)) = surface_and_feature(line) else {
            return Err(at.learn(format!("format error: {}", String::from_utf8_lossy(line))));
        };
        if surface.len() > usize::from(u16::MAX) {
            return Err(at.learn("a surface is longer than 65535 bytes"));
        }
        text.extend_from_slice(surface);
        words.push(Word {
            surface: surface.to_vec(),
            feature: feature.to_vec(),
        });
    }
    if text.is_empty() {
        return Err(learn_error(path, "empty sentence"));
    }
    if u32::try_from(text.len()).is_err() {
        return Err(learn_error(path, SENTENCE_TOO_LONG));
    }
    Ok(Some(AnswerWords { words, text }))
}

fn prefix(feature: &[u8], count: usize) -> &[u8] {
    let mut seen = 0;
    for (at, &byte) in feature.iter().enumerate() {
        if seen == count {
            return &feature[..at];
        }
        if byte == b',' {
            seen += 1;
            if seen == count {
                return &feature[..at];
            }
        }
    }
    feature
}

pub(super) fn same_node(
    answer_surface: &[u8],
    answer_feature: &[u8],
    node_surface: &[u8],
    node_feature: &[u8],
    node_is_unknown: bool,
    eval_size: usize,
    unk_eval_size: usize,
) -> bool {
    if answer_surface != node_surface {
        return false;
    }
    let size = if node_is_unknown {
        unk_eval_size
    } else {
        eval_size
    };
    prefix(answer_feature, size) == prefix(node_feature, size)
}

impl Sentence {
    pub(super) fn matches(&self, reader: &Reader, word: &Word, node: NodeId) -> bool {
        let n = &self.nodes[node.index()];
        same_node(
            &word.surface,
            &word.feature,
            self.surface(node),
            reader.features.get(n.feature),
            n.kind == UNKNOWN,
            reader.eval_size,
            reader.unk_eval_size,
        )
    }

    fn find_answer_node(&self, reader: &Reader, word: Option<&Word>, position: usize) -> NodeId {
        let mut right = NodeId::NIL;
        let mut node = self.begin_list[position];
        while node != NodeId::NIL {
            if word.is_none_or(|word| self.matches(reader, word, node)) {
                right = node;
            }
            node = self.nodes[node.index()].bnext;
        }
        right
    }

    fn find_left_path(&self, right: NodeId, left: NodeId) -> PathId {
        let mut candidate = self.nodes[right.index()].lpath;
        while candidate != PathId::NIL {
            if self.paths[candidate.index()].lnode == left {
                return candidate;
            }
            candidate = self.paths[candidate.index()].lnext;
        }
        PathId::NIL
    }

    fn count_observed(
        &self,
        reader: &Reader,
        left_path: PathId,
        right: NodeId,
        observed: &mut Vec<f64>,
    ) {
        let pool = reader.index.pool();
        let mut count = |start: PoolOffset| {
            for id in ids_from(pool, start) {
                if id >= observed.len() {
                    observed.resize(id + 1, 0.0);
                }
                observed[id] += 1.0;
            }
        };
        count(self.paths[left_path.index()].fvector);
        if self.nodes[right.index()].kind != EOS {
            count(self.nodes[right.index()].fvector);
        }
    }

    pub(super) fn link_answer(
        &mut self,
        reader: &Reader,
        words: &[Word],
        observed: &mut Vec<f64>,
        source: &FilePath,
    ) -> Result<(), Error> {
        let mut prev = self.end_list[0];
        self.nodes[prev.index()].anext = NodeId::NIL;
        let mut position = 0;
        for index in 0..=words.len() {
            let word = words.get(index);
            let right = self.find_answer_node(reader, word, position);
            if right == NodeId::NIL {
                return Err(learn_error(source, "no node for a word of the answer"));
            }
            let left_path = self.find_left_path(right, prev);
            if left_path == PathId::NIL {
                return Err(learn_error(source, "lpath is NULL"));
            }
            self.count_observed(reader, left_path, right, observed);
            self.answer.push(left_path);
            self.nodes[prev.index()].anext = right;
            prev = right;
            let Some(word) = word else {
                break;
            };
            position += word.surface.len();
        }
        let eos_head = self.begin_list[self.length];
        self.nodes[prev.index()].anext = eos_head;
        self.nodes[eos_head.index()].anext = NodeId::NIL;
        Ok(())
    }
}
