use std::ops::Deref;
use std::ops::Range;
use std::slice;
use std::str::Utf8Error;

use crate::constraint::{Constraint, ConstraintSet};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::ids::NodeId;
use crate::lattice::{Lattice, NodeSource, RawNode};

pub const DEFAULT_THETA: f32 = 0.75;

const UNKNOWN_FEATURE_FLAG: u64 = 1 << 32;
const FIXED_FEATURE_ID: u64 = 1 << 33;
const USER_FEATURE_SHIFT: u32 = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Bos,
    Eos,
    Normal,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FeatureId(u64);

fn check_text(text: &str) -> Result<(), Error> {
    if u32::try_from(text.len()).is_err() {
        return Err(Error::TextTooLong);
    }
    if text.contains('\0') {
        return Err(Error::NulInText);
    }
    Ok(())
}

pub struct Tagger<D> {
    dictionary: D,
    lattice: Lattice,
}

impl<D: Deref<Target = Dictionary>> Tagger<D> {
    pub fn new(dictionary: D) -> Self {
        Self {
            dictionary,
            lattice: Lattice::default(),
        }
    }

    pub fn parse<'t, 'x>(&'t mut self, text: &'x str) -> Result<Path<'t, 'x>, Error> {
        self.parse_with(text, &[])
    }

    pub fn parse_with<'t, 'x>(
        &'t mut self,
        text: &'x str,
        constraints: &[Constraint<'_>],
    ) -> Result<Path<'t, 'x>, Error> {
        check_text(text)?;
        self.lattice.parse(&self.dictionary, text, constraints)?;
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

    pub fn parse_lattice<'t, 'x>(
        &'t mut self,
        text: &'x str,
        constraints: &[Constraint<'_>],
    ) -> Result<LatticeView<'t, 'x>, Error> {
        check_text(text)?;
        self.lattice.parse(&self.dictionary, text, constraints)?;
        Ok(LatticeView {
            frame: Frame {
                dictionary: &self.dictionary,
                constraints: self.lattice.constraints(),
                text,
            },
            lattice: &self.lattice,
        })
    }

    pub fn parse_marginal<'t, 'x>(
        &'t mut self,
        text: &'x str,
        constraints: &[Constraint<'_>],
        theta: f32,
    ) -> Result<LatticeView<'t, 'x>, Error> {
        check_text(text)?;
        self.lattice
            .parse_marginal(&self.dictionary, text, constraints, theta)?;
        Ok(LatticeView {
            frame: Frame {
                dictionary: &self.dictionary,
                constraints: self.lattice.constraints(),
                text,
            },
            lattice: &self.lattice,
        })
    }

    pub fn parse_nbest<'t, 'x>(
        &'t mut self,
        text: &'x str,
        constraints: &[Constraint<'_>],
    ) -> Result<NBest<'t, 'x>, Error> {
        check_text(text)?;
        self.lattice
            .start_nbest(&self.dictionary, text, constraints)?;
        Ok(NBest {
            dictionary: &self.dictionary,
            lattice: &mut self.lattice,
            text,
        })
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

#[derive(Clone, Copy)]
pub struct LatticeView<'t, 'x> {
    frame: Frame<'t, 'x>,
    lattice: &'t Lattice,
}

impl<'t, 'x> LatticeView<'t, 'x> {
    fn node(&self, raw: &'t RawNode) -> Node<'t, 'x> {
        Node {
            frame: self.frame,
            raw,
        }
    }

    fn stored(&self) -> &'t [RawNode] {
        &self.lattice.nodes()[1..]
    }

    pub fn bos(&self) -> Node<'t, 'x> {
        self.node(&self.lattice.nodes()[0])
    }

    pub fn eos(&self) -> Node<'t, 'x> {
        self.node(
            self.lattice
                .nodes()
                .last()
                .expect("a lattice has an EOS node"),
        )
    }

    pub fn best_path(&self) -> Path<'t, 'x> {
        Path {
            frame: self.frame,
            nodes: self.lattice.nodes(),
            order: self.lattice.path().iter(),
            cost: self.lattice.path_cost(),
        }
    }

    pub fn is_best(&self, node: Node<'_, '_>) -> bool {
        self.index_of(node).is_some_and(|index| {
            self.lattice
                .path()
                .binary_search_by_key(&index, |id| id.index())
                .is_ok()
        })
    }

    fn index_of(&self, node: Node<'_, '_>) -> Option<usize> {
        let nodes = self.lattice.nodes();
        let offset = std::ptr::from_ref(node.raw)
            .addr()
            .checked_sub(nodes.as_ptr().addr())?;
        let index = offset / size_of::<RawNode>();
        (index < nodes.len() && offset.is_multiple_of(size_of::<RawNode>())).then_some(index)
    }

    pub fn prob(&self, node: Node<'_, '_>) -> Option<f32> {
        self.marginal(node).map(|(_, _, prob)| prob)
    }

    pub fn alpha(&self, node: Node<'_, '_>) -> Option<f32> {
        self.marginal(node).map(|(alpha, _, _)| alpha)
    }

    pub fn beta(&self, node: Node<'_, '_>) -> Option<f32> {
        self.marginal(node).map(|(_, beta, _)| beta)
    }

    pub fn z(&self) -> Option<f64> {
        self.lattice.z()
    }

    fn marginal(&self, node: Node<'_, '_>) -> Option<(f32, f32, f32)> {
        self.lattice.marginal(self.index_of(node)?)
    }

    pub fn nodes(&self) -> impl Iterator<Item = Node<'t, 'x>> + use<'t, 'x> {
        let view = *self;
        self.stored()
            .chunk_by(|a, b| a.begin() == b.begin())
            .flat_map(|group| group.iter().rev())
            .map(move |raw| view.node(raw))
    }

    pub fn begin_nodes(
        &self,
        position: usize,
    ) -> impl DoubleEndedIterator<Item = Node<'t, 'x>> + ExactSizeIterator + use<'t, 'x> {
        let stored = self.stored();
        let start = stored.partition_point(|raw| raw.begin() < position);
        let end = start + stored[start..].partition_point(|raw| raw.begin() == position);
        let view = *self;
        stored[start..end]
            .iter()
            .rev()
            .map(move |raw| view.node(raw))
    }

    pub fn end_nodes(
        &self,
        position: usize,
    ) -> impl DoubleEndedIterator<Item = Node<'t, 'x>> + ExactSizeIterator + use<'t, 'x> {
        let nodes = self.lattice.nodes();
        let view = *self;
        self.lattice
            .ending_at(position)
            .iter()
            .rev()
            .map(move |left| view.node(&nodes[left.id]))
    }
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
        Some(Node {
            frame: self.frame,
            raw: &self.nodes[id],
        })
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
            NodeSource::System | NodeSource::User(_) => NodeKind::Normal,
            NodeSource::Unknown | NodeSource::UnknownFixed | NodeSource::Constrained => {
                NodeKind::Unknown
            }
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
        self.raw.length as usize
    }

    pub fn length_with_space(&self) -> usize {
        self.raw.length_with_space as usize
    }

    pub fn feature(&self) -> Result<&'t str, Utf8Error> {
        str::from_utf8(self.feature_bytes())
    }

    pub fn feature_bytes(&self) -> &'t [u8] {
        let dictionary = self.frame.dictionary;
        match self.raw.source {
            NodeSource::System => dictionary.system.feature(self.raw.feature),
            NodeSource::User(index) => {
                dictionary.users[usize::from(index)].feature(self.raw.feature)
            }
            NodeSource::Unknown => dictionary.unknown.feature(self.raw.feature),
            NodeSource::UnknownFixed => dictionary.unknown_feature.as_deref().unwrap_or_default(),
            NodeSource::Constrained => self.frame.constraints.feature_bytes(self.raw.feature),
            NodeSource::Bos | NodeSource::Eos => &dictionary.bos_feature,
        }
    }

    pub fn feature_id(&self) -> Option<FeatureId> {
        let offset = u64::from(self.raw.feature);
        match self.raw.source {
            NodeSource::System => Some(FeatureId(offset)),
            NodeSource::User(index) => Some(FeatureId(
                ((u64::from(index) + 1) << USER_FEATURE_SHIFT) | offset,
            )),
            NodeSource::Unknown => Some(FeatureId(offset | UNKNOWN_FEATURE_FLAG)),
            NodeSource::UnknownFixed => Some(FeatureId(FIXED_FEATURE_ID)),
            NodeSource::Constrained | NodeSource::Bos | NodeSource::Eos => None,
        }
    }

    pub fn left_id(&self) -> u16 {
        self.raw.left_id
    }

    pub fn right_id(&self) -> u16 {
        self.raw.right_id
    }

    pub fn pos_id(&self) -> u16 {
        self.raw.pos_id
    }

    pub fn char_type(&self) -> u8 {
        self.raw.char_type
    }

    pub fn word_cost(&self) -> i16 {
        self.raw.word_cost
    }

    pub fn cost(&self) -> i32 {
        self.raw.cost
    }
}
