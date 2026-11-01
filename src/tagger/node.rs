use std::fmt;
use std::ops::Range;

use super::Frame;
use crate::feature_text::FeatureText;
use crate::lattice::{NodeSource, RawNode};

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

#[derive(Clone, Copy)]
pub struct Node<'t, 'x> {
    pub(super) frame: Frame<'t, 'x>,
    pub(super) raw: &'t RawNode,
}

impl fmt::Debug for Node<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Node")
            .field("kind", &self.kind())
            .field("surface", &self.surface())
            .field("surface_range", &self.surface_range())
            .field("feature", &self.feature())
            .field("feature_id", &self.feature_id())
            .field("left_id", &self.left_id())
            .field("right_id", &self.right_id())
            .field("word_cost", &self.word_cost())
            .field("cost", &self.cost())
            .finish_non_exhaustive()
    }
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
            NodeSource::Unknown => FeatureText::packed(&dictionary.system, self.raw.feature, ""),
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
