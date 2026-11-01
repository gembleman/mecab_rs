use std::fs::File;
use std::ops::Range;
use std::path::Path;

use super::CHUNK;
use crate::error::{Error, IoAction, io_error};
use crate::packed::SECTIONS;

#[cfg(unix)]
pub(super) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
}

#[cfg(windows)]
pub(super) fn read_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        let read = std::os::windows::fs::FileExt::seek_read(file, buffer, offset)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer = &mut buffer[read..];
        offset += read as u64;
    }
    Ok(())
}

pub(in crate::packed) struct Source<'a> {
    pub(super) file: &'a File,
    pub(super) path: &'a Path,
    pub(super) sections: [Range<usize>; SECTIONS],
}

impl Source<'_> {
    pub(in crate::packed) fn broken(&self, reason: &'static str) -> Error {
        Error::format(self.path, reason)
    }

    pub(in crate::packed) fn section_length(&self, section: usize) -> usize {
        self.sections[section].len()
    }

    pub(in crate::packed) fn read(
        &self,
        section: usize,
        from: usize,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        let range = &self.sections[section];
        let inside = from
            .checked_add(buffer.len())
            .is_some_and(|end| end <= range.len());
        if !inside {
            return Err(self.broken("a read leaves its section"));
        }
        let start = range.start + from;
        read_at(self.file, buffer, start as u64).map_err(io_error(IoAction::Read, self.path))
    }

    pub(super) fn read_all(&self, section: usize) -> Result<Vec<u8>, Error> {
        let mut bytes = vec![0; self.sections[section].len()];
        self.read(section, 0, &mut bytes)?;
        Ok(bytes)
    }

    pub(super) fn chunks(
        &self,
        section: usize,
        unit: usize,
        mut visit: impl FnMut(usize, &[u8]) -> bool,
        reason: &'static str,
    ) -> Result<(), Error> {
        let length = self.sections[section].len();
        let step = CHUNK / unit * unit;
        let mut buffer = vec![0; step.min(length)];
        let mut done = 0;
        while done < length {
            let part = step.min(length - done);
            self.read(section, done, &mut buffer[..part])?;
            if !visit(done / unit, &buffer[..part]) {
                return Err(self.broken(reason));
            }
            done += part;
        }
        Ok(())
    }
}
