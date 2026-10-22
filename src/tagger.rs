use std::ops::Range;
use std::slice;
use std::str::Utf8Error;
use std::ops::Deref;

use crate::constraint::{Constraint, ConstraintSet};
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

pub struct Tagger<D> {
    dictionary: D,
    lattice: Lattice,
}

impl<D: Deref<Target = Dictionary>> Tagger<D> {
    pub fn new(dictionary: D) -> Self {
        Self { dictionary, lattice: Lattice::default() }
    }

    pub fn parse<'t, 'x>(&'t mut self, text: &'x str) -> Result<Path<'t, 'x>, Error> {
        self.parse_with(text, &[], 1)
    }

    pub fn parse_with<'t, 'x>(
        &'t mut self,
        text: &'x str,
        constraints: &[Constraint<'_>],
        rank: usize,
    ) -> Result<Path<'t, 'x>, Error> {
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
            cost: self.lattice.path_cost(),
        })
    }

    pub fn parse_nbest<'t, 'x>(
        &'t mut self,
        text: &'x str,
        constraints: &[Constraint<'_>],
    ) -> Result<NBest<'t, 'x>, Error> {
        if text.contains('\0') {
            return Err(Error::NulInText);
        }
        self.lattice.start_nbest(&self.dictionary, text, constraints)?;
        Ok(NBest { dictionary: &self.dictionary, lattice: &mut self.lattice, text })
    }
}

pub struct NBest<'t, 'x> {
    dictionary: &'t Dictionary,
    lattice: &'t mut Lattice,
    text: &'x str,
}

impl<'x> NBest<'_, 'x> {
    pub fn next_path(&mut self) -> Option<Path<'_, 'x>> {
        if !self.lattice.next_nbest_path() {
            return None;
        }
        Some(Path {
            frame: Frame {
                dictionary: self.dictionary,
                constraints: self.lattice.constraints(),
                text: self.text,
            },
            nodes: self.lattice.nodes(),
            order: self.lattice.path().iter(),
            cost: self.lattice.path_cost(),
        })
    }
}

#[derive(Clone, Copy)]
struct Frame<'t, 'x> {
    dictionary: &'t Dictionary,
    constraints: &'t ConstraintSet,
    text: &'x str,
}

pub struct Path<'t, 'x> {
    frame: Frame<'t, 'x>,
    nodes: &'t [RawNode],
    order: slice::Iter<'t, NodeId>,
    cost: i64,
}

impl Path<'_, '_> {
    pub fn cost(&self) -> i64 {
        self.cost
    }
}

impl<'t, 'x> Iterator for Path<'t, 'x> {
    type Item = Node<'t, 'x>;

    fn next(&mut self) -> Option<Node<'t, 'x>> {
        let &id = self.order.next()?;
        Some(Node { frame: self.frame, raw: &self.nodes[id] })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.order.size_hint()
    }
}

impl ExactSizeIterator for Path<'_, '_> {}

#[derive(Clone, Copy)]
pub struct Node<'t, 'x> {
    frame: Frame<'t, 'x>,
    raw: &'t RawNode,
}

impl<'t, 'x> Node<'t, 'x> {
    pub fn kind(&self) -> NodeKind {
        match self.raw.source {
            NodeSource::System => NodeKind::Normal,
            NodeSource::Unknown | NodeSource::Constrained => NodeKind::Unknown,
            NodeSource::Bos => NodeKind::Bos,
            NodeSource::Eos => NodeKind::Eos,
        }
    }

    pub fn surface(&self) -> &'x str {
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

    pub fn feature(&self) -> Result<&'t str, Utf8Error> {
        str::from_utf8(self.feature_bytes())
    }

    pub fn feature_bytes(&self) -> &'t [u8] {
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
