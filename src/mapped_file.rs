use std::fs::File;
use std::ops::Deref;
use std::path::Path;

use memmap2::Mmap;

use crate::error::{Error, IoAction, io_error};

#[cfg(windows)]
const SHARE_READ_AND_DELETE: u32 = 1 | 4;

pub(crate) fn open_unwritable(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, SHARE_READ_AND_DELETE);
    options.open(path)
}

pub struct MappedFile {
    map: Mmap,
    file: File,
}

impl MappedFile {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let file = open_unwritable(path).map_err(io_error(IoAction::Open, path))?;
        Self::map(file, path)
    }

    pub(crate) fn map(file: File, path: &Path) -> Result<Self, Error> {
        let map = unsafe { Mmap::map(&file) }.map_err(io_error(IoAction::Map, path))?;
        Ok(Self { map, file })
    }

    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.map
    }
}
