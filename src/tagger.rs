use std::ops::Range;
use std::slice;
use std::sync::Arc;

use crate::constraint::{ConstraintSet, Constraints};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::feature_text::FeatureText;
use crate::ids::NodeId;
use crate::lattice::{Lattice, MAX_TEXT_LEN, NodeSource, RawNode};
use crate::partial::PartialInput;

pub const DEFAULT_THETA: f32 = 0.75;

const FEATURE_OFFSET_BITS: u32 = 32;
const UNKNOWN_FEATURE_ID_BIT: u64 = 1 << FEATURE_OFFSET_BITS;
const UNKNOWN_FIXED_FEATURE_ID: u64 = 1 << (FEATURE_OFFSET_BITS + 1);
const USER_DICTIONARY_ID_SHIFT: u32 = FEATURE_OFFSET_BITS + 8;

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
    if text.len() > MAX_TEXT_LEN {
        return Err(Error::TextTooLong);
    }
    if text.contains('\0') {
        return Err(Error::NulInText);
    }
    Ok(())
}

mod sealed {
    pub trait Sealed {}
}

pub trait DictionaryHandle: sealed::Sealed {
    fn dictionary(&self) -> &Dictionary;
}

impl sealed::Sealed for &Dictionary {}

impl DictionaryHandle for &Dictionary {
    fn dictionary(&self) -> &Dictionary {
        self
    }
}

impl sealed::Sealed for Arc<Dictionary> {}

impl DictionaryHandle for Arc<Dictionary> {
    fn dictionary(&self) -> &Dictionary {
        self
    }
}

pub struct Tagger<D> {
    dictionary: D,
    lattice: Lattice,
}

impl<D: DictionaryHandle> Tagger<D> {
    pub fn new(dictionary: D) -> Self {
        Self {
            dictionary,
            lattice: Lattice::default(),
        }
    }

    fn run_on(
        &mut self,
        text: &str,
        run: impl FnOnce(&mut Lattice, &Dictionary) -> Result<(), Error>,
    ) -> Result<(), Error> {
        check_text(text)?;
        run(&mut self.lattice, self.dictionary.dictionary())
    }

    fn frame<'t, 'x>(&'t self, text: &'x str) -> Frame<'t, 'x> {
        Frame {
            dictionary: self.dictionary.dictionary(),
            constraints: self.lattice.constraints(),
            text,
        }
    }

    fn view<'t, 'x>(&'t self, text: &'x str) -> LatticeView<'t, 'x> {
        LatticeView {
            frame: self.frame(text),
            lattice: &self.lattice,
        }
    }

    pub fn parse<'t, 'x>(&'t mut self, text: &'x str) -> Result<Path<'t, 'x>, Error> {
        self.parse_with(&Constraints::new(text))
    }

    pub fn parse_with<'t, 'x>(
        &'t mut self,
        constraints: &Constraints<'x>,
    ) -> Result<Path<'t, 'x>, Error> {
        let text = constraints.text();
        self.run_on(text, |lattice, dictionary| {
            lattice.parse_best(dictionary, text, constraints.as_slice())
        })?;
        Ok(self.view(text).best_path())
    }

    pub fn parse_partial<'t, 'x>(
        &'t mut self,
        input: &'x PartialInput<'_>,
    ) -> Result<Path<'t, 'x>, Error> {
        self.parse_with(&input.constraints())
    }

    pub fn parse_lattice<'t, 'x>(
        &'t mut self,
        constraints: &Constraints<'x>,
    ) -> Result<LatticeView<'t, 'x>, Error> {
        let text = constraints.text();
        self.run_on(text, |lattice, dictionary| {
            lattice.parse(dictionary, text, constraints.as_slice())
        })?;
        Ok(self.view(text))
    }

    pub fn parse_marginal<'t, 'x>(
        &'t mut self,
        constraints: &Constraints<'x>,
        theta: f32,
    ) -> Result<LatticeView<'t, 'x>, Error> {
        if !(theta.is_finite() && theta >= 0.0) {
            return Err(Error::InvalidTheta { theta });
        }
        let text = constraints.text();
        self.run_on(text, |lattice, dictionary| {
            lattice.parse_marginal(dictionary, text, constraints.as_slice(), theta)
        })?;
        Ok(self.view(text))
    }

    pub fn parse_nbest<'t, 'x>(
        &'t mut self,
        constraints: &Constraints<'x>,
    ) -> Result<NBest<'t, 'x>, Error> {
        let text = constraints.text();
        self.run_on(text, |lattice, dictionary| {
            lattice.start_nbest(dictionary, text, constraints.as_slice())
        })?;
        Ok(NBest {
            dictionary: self.dictionary.dictionary(),
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

    fn nodes_after_bos(&self) -> &'t [RawNode] {
        &self.lattice.nodes()[1..]
    }

    pub fn bos(&self) -> Node<'t, 'x> {
        debug_assert!(!self.lattice.nodes().is_empty());
        self.node(&self.lattice.nodes()[0])
    }

    pub fn eos(&self) -> Node<'t, 'x> {
        debug_assert!(!self.lattice.nodes().is_empty());
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
        debug_assert!(
            self.lattice.path().is_sorted_by_key(|id| id.index()),
            "the best path is in ascending node order"
        );
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
        self.nodes_after_bos()
            .chunk_by(|a, b| a.begin() == b.begin())
            .flat_map(|group| group.iter().rev())
            .map(move |raw| view.node(raw))
    }

    pub fn begin_nodes(
        &self,
        position: usize,
    ) -> impl DoubleEndedIterator<Item = Node<'t, 'x>> + ExactSizeIterator + use<'t, 'x> {
        let after_bos = self.nodes_after_bos();
        let start = after_bos.partition_point(|raw| raw.begin() < position);
        let end = start + after_bos[start..].partition_point(|raw| raw.begin() == position);
        let view = *self;
        after_bos[start..end]
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

impl<'x> Node<'_, 'x> {
    pub fn kind(&self) -> NodeKind {
        match self.raw.source {
            NodeSource::System | NodeSource::SystemExtended | NodeSource::User(_) => {
                NodeKind::Normal
            }
            NodeSource::Unknown | NodeSource::UnknownFixed | NodeSource::Constrained => {
                NodeKind::Unknown
            }
            NodeSource::Bos => NodeKind::Bos,
            NodeSource::Eos => NodeKind::Eos,
        }
    }

    pub fn surface(&self) -> &'x str {
        &self.frame.text[self.surface_range()]
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

    pub fn feature(&self) -> FeatureText<'_> {
        let dictionary = self.frame.dictionary;
        match self.raw.source {
            NodeSource::System => {
                FeatureText::packed(&dictionary.system, self.raw.feature, self.surface())
            }
            NodeSource::SystemExtended => FeatureText::packed(
                &dictionary.system,
                self.raw.feature,
                dictionary.system.key_of(self.surface(), self.raw.feature),
            ),
            NodeSource::User(index) => {
                FeatureText::text(dictionary.users[index.position()].feature(self.raw.feature))
            }
            NodeSource::Unknown => FeatureText::packed(&dictionary.unknown, self.raw.feature, ""),
            NodeSource::UnknownFixed => {
                FeatureText::text(dictionary.unknown_feature.as_deref().unwrap_or_default())
            }
            NodeSource::Constrained => {
                FeatureText::text(self.frame.constraints.feature_str(self.raw.feature))
            }
            NodeSource::Bos | NodeSource::Eos => FeatureText::text(&dictionary.bos_feature),
        }
    }

    pub fn feature_id(&self) -> Option<FeatureId> {
        let offset = u64::from(self.raw.feature);
        match self.raw.source {
            NodeSource::System | NodeSource::SystemExtended => Some(FeatureId(offset)),
            NodeSource::User(index) => Some(FeatureId(
                (index.number() << USER_DICTIONARY_ID_SHIFT) | offset,
            )),
            NodeSource::Unknown => Some(FeatureId(offset | UNKNOWN_FEATURE_ID_BIT)),
            NodeSource::UnknownFixed => Some(FeatureId(UNKNOWN_FIXED_FEATURE_ID)),
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
