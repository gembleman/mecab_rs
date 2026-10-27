use std::fmt;

use crate::packed::PackedDictionary;

#[derive(Clone, Copy)]
pub struct FeatureText<'a>(Source<'a>);

#[derive(Clone, Copy)]
enum Source<'a> {
    Text(&'a str),
    Packed(&'a PackedDictionary, u32, &'a str),
}

impl<'a> FeatureText<'a> {
    pub(crate) fn text(text: &'a str) -> Self {
        Self(Source::Text(text))
    }

    pub(crate) fn packed(dictionary: &'a PackedDictionary, offset: u32, key: &'a str) -> Self {
        Self(Source::Packed(dictionary, offset, key))
    }

    #[inline]
    pub fn write_to(&self, out: &mut String) {
        match self.0 {
            Source::Text(text) => out.push_str(text),
            Source::Packed(dictionary, offset, key) => unsafe {
                dictionary.feature_view().emit_into(offset, key, out)
            },
        }
    }
}

impl fmt::Display for FeatureText<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Source::Text(text) => formatter.write_str(text),
            Source::Packed(dictionary, offset, key) => {
                dictionary.feature_view().emit(offset, key, formatter)
            }
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
