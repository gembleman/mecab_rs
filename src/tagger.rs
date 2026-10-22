use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::Range;
use std::slice;
use std::str::Utf8Error;
use std::sync::Arc;

use crate::constraint::{ConstraintSet, Constraint};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::ids::NodeId;
use crate::lattice::{Lattice, NodeSource, RawNode};

const UNKNOWN_FEATURE_FLAG: u64 = 1 << 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Bos,
    Eos,
    Normal,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FeatureId(u64);

pub struct Tagger {
    dictionary: Arc<Dictionary>,
    lattice: Lattice,
    not_sync: PhantomData<Cell<()>>,
}

impl Tagger {
    pub fn new(dictionary: Arc<Dictionary>) -> Self {
        Self { dictionary, lattice: Lattice::default(), not_sync: PhantomData }
    }

    pub fn parse<'a>(&'a mut self, text: &'a str) -> Result<Path<'a>, Error> {
        self.parse_with(text, &[], 1)
    }

    pub fn parse_with<'a>(
        &'a mut self,
        text: &'a str,
        constraints: &[Constraint],
        rank: usize,
    ) -> Result<Path<'a>, Error> {
        if text.contains('\0') {
            return Err(Error::NulInText);
        }
        self.lattice.parse(&self.dictionary, text, constraints, rank)?;
        Ok(Path {
            frame: Frame {
                dictionary: &self.dictionary,
                constraints: self.lattice.constraints(),
                text,
            },
            nodes: self.lattice.nodes(),
            order: self.lattice.path().iter(),
        })
    }
}

#[derive(Clone, Copy)]
struct Frame<'a> {
    dictionary: &'a Dictionary,
    constraints: &'a ConstraintSet,
    text: &'a str,
}

pub struct Path<'a> {
    frame: Frame<'a>,
    nodes: &'a [RawNode],
    order: slice::Iter<'a, NodeId>,
}

impl<'a> Iterator for Path<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        let &id = self.order.next()?;
        Some(Node { frame: self.frame, raw: &self.nodes[id] })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.order.size_hint()
    }
}

impl ExactSizeIterator for Path<'_> {}

#[derive(Clone, Copy)]
pub struct Node<'a> {
    frame: Frame<'a>,
    raw: &'a RawNode,
}

impl<'a> Node<'a> {
    pub fn kind(&self) -> NodeKind {
        match self.raw.source {
            NodeSource::System => NodeKind::Normal,
            NodeSource::Unknown | NodeSource::Constrained => NodeKind::Unknown,
            NodeSource::Bos => NodeKind::Bos,
            NodeSource::Eos => NodeKind::Eos,
        }
    }

    pub fn surface(&self) -> &'a str {
        self.frame.text.get(self.surface_range()).unwrap_or("")
    }

    pub fn surface_range(&self) -> Range<usize> {
        let start = self.raw.surface_start as usize;
        start..start + self.length()
    }

    pub fn length(&self) -> usize {
        usize::from(self.raw.length)
    }

    pub fn rlength(&self) -> usize {
        usize::from(self.raw.rlength)
    }

    pub fn feature(&self) -> Result<&'a str, Utf8Error> {
        str::from_utf8(self.feature_bytes())
    }

    pub fn feature_bytes(&self) -> &'a [u8] {
        let dictionary = self.frame.dictionary;
        match self.raw.source {
            NodeSource::System => dictionary.system.feature(self.raw.feature),
            NodeSource::Unknown => dictionary.unknown.feature(self.raw.feature),
            NodeSource::Constrained => self.frame.constraints.feature_bytes(self.raw.feature),
            NodeSource::Bos | NodeSource::Eos => &dictionary.bos_feature,
        }
    }

    pub fn feature_id(&self) -> Option<FeatureId> {
        let offset = u64::from(self.raw.feature);
        match self.raw.source {
            NodeSource::System => Some(FeatureId(offset)),
            NodeSource::Unknown => Some(FeatureId(offset | UNKNOWN_FEATURE_FLAG)),
            NodeSource::Constrained | NodeSource::Bos | NodeSource::Eos => None,
        }
    }

    pub fn left_id(&self) -> u16 {
        self.raw.lc_attr
    }

    pub fn right_id(&self) -> u16 {
        self.raw.rc_attr
    }

    pub fn pos_id(&self) -> u16 {
        self.raw.pos_id
    }

    pub fn char_type(&self) -> u8 {
        self.raw.char_type
    }

    pub fn word_cost(&self) -> i16 {
        self.raw.wcost
    }

    pub fn cost(&self) -> i32 {
        self.raw.cost
    }
}
