use std::fmt;
use std::sync::Arc;

use crate::constraint::{ConstraintSet, Constraints};
use crate::dictionary::Dictionary;
use crate::error::Error;
use crate::lattice::{Lattice, MAX_TEXT_LEN};
use crate::partial::PartialInput;

mod nbest;
mod node;
mod path;
mod view;

pub use nbest::NBest;
pub use node::{FeatureId, Node, NodeKind};
pub use path::Path;
pub use view::LatticeView;

pub const DEFAULT_THETA: f32 = 0.75;

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

impl<D: fmt::Debug> fmt::Debug for Tagger<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Tagger")
            .field("dictionary", &self.dictionary)
            .finish_non_exhaustive()
    }
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

#[derive(Clone, Copy)]
struct Frame<'t, 'x> {
    dictionary: &'t Dictionary,
    constraints: &'t ConstraintSet,
    text: &'x str,
}
