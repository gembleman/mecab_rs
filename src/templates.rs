use std::io::{self, Write};
use std::mem::size_of;
use std::path::Path;

use crate::error::Error;
use crate::section::{Plain, Section};

pub(crate) const TEMPLATE_HEADER_PEEK_BYTES: usize = 24;
const SIZES_OFFSET: usize = 16;
const WORD_BYTES: usize = 4;
const ARRAY_ALIGNMENT: usize = 8;

pub(crate) fn header_sizes(header: &[u8]) -> Option<(usize, usize)> {
    let word = |at: usize| {
        let bytes = header.get(at..at + WORD_BYTES)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?) as usize)
    };
    Some((word(SIZES_OFFSET)?, word(SIZES_OFFSET + WORD_BYTES)?))
}

fn padding_after(bytes: usize) -> usize {
    bytes.next_multiple_of(ARRAY_ALIGNMENT) - bytes
}

pub(crate) fn write_array<T: Copy, const WIDTH: usize>(
    output: &mut impl Write,
    items: &[T],
    encode: fn(T) -> [u8; WIDTH],
) -> io::Result<()> {
    for item in items {
        output.write_all(&encode(*item))?;
    }
    output.write_all(&[0; ARRAY_ALIGNMENT][..padding_after(items.len() * WIDTH)])
}

pub(crate) struct TemplateReader<'a> {
    pub(crate) file: &'a [u8],
    path: &'a Path,
    offset: usize,
}

impl<'a> TemplateReader<'a> {
    pub(crate) fn new(file: &'a [u8], path: &'a Path) -> Self {
        Self {
            file,
            path,
            offset: 0,
        }
    }

    pub(crate) fn at_end(&self) -> bool {
        self.offset == self.file.len()
    }

    pub(crate) fn broken(&self, reason: &'static str) -> Error {
        Error::format(self.path, reason)
    }

    pub(crate) fn bytes<const WIDTH: usize>(&mut self) -> Result<[u8; WIDTH], Error> {
        let end = self.offset + WIDTH;
        let bytes = self
            .file
            .get(self.offset..end)
            .ok_or_else(|| self.broken("the file ends inside a header"))?;
        self.offset = end;
        Ok(bytes.try_into().expect("the slice has the requested width"))
    }

    pub(crate) fn count(&mut self) -> Result<usize, Error> {
        self.bytes().map(|bytes| u32::from_le_bytes(bytes) as usize)
    }

    pub(crate) fn copied<T, const WIDTH: usize>(
        &mut self,
        count: usize,
        decode: fn([u8; WIDTH]) -> T,
    ) -> Result<Vec<T>, Error> {
        let length = count
            .checked_mul(WIDTH)
            .ok_or_else(|| self.broken("an array is too long"))?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| self.broken("an array is too long"))?;
        let bytes = self
            .file
            .get(self.offset..end)
            .ok_or_else(|| self.broken("the file ends inside an array"))?;
        let items = bytes
            .as_chunks::<WIDTH>()
            .0
            .iter()
            .map(|&item| decode(item))
            .collect();
        self.offset = end + padding_after(length);
        Ok(items)
    }

    pub(crate) fn section<T: Plain>(&mut self, count: usize) -> Result<Section<T>, Error> {
        let length = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| self.broken("an array is too long"))?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| self.broken("an array is too long"))?;
        let section = Section::new(self.file, self.offset..end)
            .ok_or_else(|| self.broken("an array is misplaced"))?;
        self.offset = end + padding_after(length);
        Ok(section)
    }
}
