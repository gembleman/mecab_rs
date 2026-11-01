use std::fmt;

use super::{Frame, Path};
use crate::dictionary::Dictionary;
use crate::lattice::Lattice;

pub struct NBest<'t, 'x> {
    pub(super) dictionary: &'t Dictionary,
    pub(super) lattice: &'t mut Lattice,
    pub(super) text: &'x str,
}

impl fmt::Debug for NBest<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NBest")
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
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
