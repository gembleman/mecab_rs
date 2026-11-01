const WILDCARD: &[u8] = b"*";
const QUOTE: u8 = b'"';
const SEPARATOR: u8 = b',';

pub(super) fn matches_every_feature(constraint: &[u8]) -> bool {
    constraint == WILDCARD
}

pub(super) fn partial_match(constraint: &[u8], feature: &[u8]) -> bool {
    if matches_every_feature(constraint) {
        return true;
    }
    Columns::new(constraint)
        .zip(Columns::new(feature))
        .all(|(wanted, actual)| {
            wanted.bytes().eq(WILDCARD.iter().copied()) || wanted.bytes().eq(actual.bytes())
        })
}

#[derive(Clone, Copy)]
struct Column<'a> {
    content: &'a [u8],
    quoted: bool,
}

impl<'a> Column<'a> {
    fn bytes(self) -> impl Iterator<Item = u8> + 'a {
        let mut rest = self.content.iter().copied();
        std::iter::from_fn(move || {
            let byte = rest.next()?;
            if self.quoted && byte == QUOTE {
                rest.next();
            }
            Some(byte)
        })
    }
}

struct Columns<'a> {
    source: &'a [u8],
    at: usize,
}

impl<'a> Columns<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self { source, at: 0 }
    }

    fn separator_from(&self, from: usize) -> usize {
        let tail = &self.source[from..];
        tail.iter()
            .position(|&byte| byte == SEPARATOR)
            .map_or(self.source.len(), |o| from + o)
    }

    fn closing_quote_from(&self, from: usize) -> usize {
        let mut cursor = from;
        while let Some(&byte) = self.source.get(cursor) {
            if byte == QUOTE {
                if self.source.get(cursor + 1) != Some(&QUOTE) {
                    break;
                }
                cursor += 1;
            }
            cursor += 1;
        }
        cursor
    }
}

impl<'a> Iterator for Columns<'a> {
    type Item = Column<'a>;

    fn next(&mut self) -> Option<Column<'a>> {
        if self.at >= self.source.len() {
            return None;
        }
        while matches!(self.source.get(self.at), Some(b' ' | b'\t')) {
            self.at += 1;
        }
        let column;
        if self.source.get(self.at) == Some(&QUOTE) {
            let start = self.at + 1;
            let closing = self.closing_quote_from(start);
            column = Column {
                content: &self.source[start..closing],
                quoted: true,
            };
            self.at = self.separator_from((closing + 1).min(self.source.len()));
        } else {
            let end = self.separator_from(self.at);
            column = Column {
                content: &self.source[self.at..end],
                quoted: false,
            };
            self.at = end;
        }
        self.at += 1;
        Some(column)
    }
}
