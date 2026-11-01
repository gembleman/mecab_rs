use std::path::Path as FilePath;

use super::answer::Word;
use super::{Node, NodeId, Path, PathId, Reader, Sentence, offset};
use crate::learn::features::{PoolOffset, widen};
use crate::learn::tokenizer::{BOS, EOS, NORMAL};
use crate::tool_error::learn_error;
use mecab_rs::Error;

const TEXT_PADDING: usize = 8;
const TOO_MANY_NODES: &str = "a sentence has too many nodes";
const TOO_MANY_PATHS: &str = "a sentence has too many paths";

impl Sentence {
    fn node(&mut self, node: Node, source: &FilePath) -> Result<NodeId, Error> {
        let index =
            NodeId::new(self.nodes.len()).ok_or_else(|| learn_error(source, TOO_MANY_NODES))?;
        self.nodes.push(node);
        Ok(index)
    }

    fn lookup(
        &mut self,
        reader: &mut Reader,
        position: usize,
        source: &FilePath,
    ) -> Result<NodeId, Error> {
        if self.begin_list[position] != NodeId::NIL {
            return Ok(self.begin_list[position]);
        }
        reader
            .tokenizer
            .lookup(&self.text, position, self.length, &mut reader.found);
        let mut head = NodeId::NIL;
        for index in 0..reader.found.len() {
            let found = reader.found[index];
            let feature = reader.tokenizer.feature(found.feature);
            let mut node = Node::new(found.kind, reader.features.intern(feature)?);
            node.position = offset(found.position);
            node.length = offset(found.length);
            node.rlength = offset(found.rlength);
            node.char_type = found.char_type;
            node.bnext = head;
            head = self.node(node, source)?;
        }
        self.begin_list[position] = head;
        Ok(head)
    }

    fn connect(
        &mut self,
        reader: &mut Reader,
        position: usize,
        first: NodeId,
        source: &FilePath,
    ) -> Result<(), Error> {
        let mut right = first;
        while right != NodeId::NIL {
            let mut left = self.end_list[position];
            while left != NodeId::NIL {
                let path = PathId::new(self.paths.len())
                    .ok_or_else(|| learn_error(source, TOO_MANY_PATHS))?;
                self.paths.push(Path {
                    rnode: right,
                    lnode: left,
                    rnext: self.nodes[left.index()].rpath,
                    lnext: self.nodes[right.index()].lpath,
                    fvector: PoolOffset::NONE,
                    cost: 0.0,
                });
                self.nodes[right.index()].lpath = path;
                self.nodes[left.index()].rpath = path;
                self.build_feature(reader, path)?;
                left = self.nodes[left.index()].enext;
            }
            let end = widen(self.nodes[right.index()].rlength) + position;
            self.nodes[right.index()].enext = self.end_list[end];
            self.end_list[end] = right;
            right = self.nodes[right.index()].bnext;
        }
        Ok(())
    }

    fn build_feature(&mut self, reader: &mut Reader, path: PathId) -> Result<(), Error> {
        let (left, right) = {
            let path = &self.paths[path.index()];
            (path.lnode, path.rnode)
        };
        self.nodes[right.index()].wcost = 0.0;
        self.paths[path.index()].cost = 0.0;
        let left_feature = self.nodes[left.index()].feature;
        let right_feature = self.nodes[right.index()].feature;
        reader.features.rewrite(left_feature)?;
        reader.features.rewrite(right_feature)?;
        let right_node = &self.nodes[right.index()];
        let surface: &[u8] = if right_node.kind == NORMAL {
            &self.text[right_node.start()..right_node.end()]
        } else {
            b""
        };
        let unigram = reader.index.unigram(
            &reader.features.rewritten(right_feature).unigram,
            right_node.char_type,
            surface,
        )?;
        self.nodes[right.index()].fvector = unigram;
        let bigram = reader.index.bigram(
            &reader.features.rewritten(left_feature).right,
            &reader.features.rewritten(right_feature).left,
        )?;
        self.paths[path.index()].fvector = bigram;
        Ok(())
    }

    pub(super) fn build_lattice(
        &mut self,
        reader: &mut Reader,
        source: &FilePath,
    ) -> Result<(), Error> {
        for position in 0..=self.length {
            if self.end_list[position] == NodeId::NIL {
                continue;
            }
            let first = self.lookup(reader, position, source)?;
            self.connect(reader, position, first, source)?;
        }
        if self.end_list[self.length] == NodeId::NIL {
            self.begin_list[self.length] = self.lookup(reader, self.length, source)?;
            for position in (0..=self.length).rev() {
                if self.end_list[position] != NodeId::NIL {
                    let first = self.begin_list[self.length];
                    self.connect(reader, position, first, source)?;
                    break;
                }
            }
        }
        Ok(())
    }

    pub(super) fn with_text(mut text: Vec<u8>) -> Self {
        let length = text.len();
        text.extend_from_slice(&[0; TEXT_PADDING]);
        Self {
            text,
            length,
            nodes: Vec::new(),
            paths: Vec::new(),
            begin_list: vec![NodeId::NIL; length + TEXT_PADDING],
            end_list: vec![NodeId::NIL; length + TEXT_PADDING],
            answer: Vec::new(),
        }
    }

    pub(super) fn add_boundary_nodes(
        &mut self,
        reader: &mut Reader,
        source: &FilePath,
    ) -> Result<(), Error> {
        let bos_feature = reader.features.intern(&reader.tokenizer.bos_feature)?;
        let bos = self.node(Node::new(BOS, bos_feature), source)?;
        let eos = self.node(Node::new(EOS, bos_feature), source)?;
        self.end_list[0] = bos;
        self.begin_list[self.length] = eos;
        Ok(())
    }

    fn has_matching_node(
        &mut self,
        reader: &mut Reader,
        word: &Word,
        position: usize,
        source: &FilePath,
    ) -> Result<bool, Error> {
        let mut node = self.lookup(reader, position, source)?;
        while node != NodeId::NIL {
            if self.matches(reader, word, node) {
                return Ok(true);
            }
            node = self.nodes[node.index()].bnext;
        }
        Ok(false)
    }

    fn add_virtual_node(
        &mut self,
        reader: &mut Reader,
        word: &Word,
        position: usize,
        source: &FilePath,
    ) -> Result<(), Error> {
        let mut node = Node::new(NORMAL, reader.features.intern(&word.feature)?);
        node.position = offset(position);
        node.length = offset(word.surface.len());
        node.rlength = offset(word.surface.len());
        node.bnext = self.begin_list[position];
        let node = self.node(node, source)?;
        self.begin_list[position] = node;
        Ok(())
    }

    pub(super) fn add_virtual_nodes(
        &mut self,
        reader: &mut Reader,
        words: &[Word],
        source: &FilePath,
    ) -> Result<Vec<Vec<u8>>, Error> {
        let mut virtual_features = Vec::new();
        let mut position = 0;
        for word in words {
            if !self.has_matching_node(reader, word, position, source)? {
                self.add_virtual_node(reader, word, position, source)?;
                virtual_features.push(word.feature.clone());
            }
            position += word.surface.len();
        }
        Ok(virtual_features)
    }
}
