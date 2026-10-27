use std::fmt;

use crate::packed::PackedDictionary;

#[derive(Clone, Copy)]
pub struct FeatureText<'a>(Source<'a>);

#[derive(Clone, Copy)]
enum Source<'a> {
    Text(&'a str),
    Packed(&'a PackedDictionary, u32),
}

impl<'a> FeatureText<'a> {
    pub(crate) fn text(text: &'a str) -> Self {
        Self(Source::Text(text))
    }

    pub(crate) fn packed(dictionary: &'a PackedDictionary, offset: u32) -> Self {
        Self(Source::Packed(dictionary, offset))
    }

    #[inline]
    pub fn write_to(&self, out: &mut String) {
        match self.0 {
            Source::Text(text) => out.push_str(text),
            Source::Packed(dictionary, offset) => dictionary.write_feature(offset, out),
        }
    }
}

impl fmt::Display for FeatureText<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Source::Text(text) => formatter.write_str(text),
            Source::Packed(dictionary, offset) => dictionary.feature_view().emit(offset, formatter),
        }
    }
}

impl fmt::Debug for FeatureText<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = String::new();
        self.write_to(&mut text);
        fmt::Debug::fmt(&text, formatter)
    }
}
