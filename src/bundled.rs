use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::dictionary::Dictionary;
use crate::error::{Error, IoAction, io_error};
use crate::file_write::write_file;

const CACHE_FOLDER: &str = "mecab_rs";

#[derive(Clone, Copy, Debug)]
pub struct Bundled {
    file_name: &'static str,
    bytes: &'static [u8],
}

#[cfg(feature = "unidic")]
pub const UNIDIC: Bundled = Bundled {
    file_name: mecab_dict_unidic::FILE_NAME,
    bytes: mecab_dict_unidic::MRD_XZ,
};

#[cfg(feature = "ipadic")]
pub const IPADIC: Bundled = Bundled {
    file_name: mecab_dict_ipadic::FILE_NAME,
    bytes: mecab_dict_ipadic::MRD_XZ,
};

impl Bundled {
    pub fn open(&self) -> Result<Dictionary, Error> {
        let folder = dirs::cache_dir()
            .ok_or(Error::Unsupported(
                "this platform has no cache folder; use `open_in` with a folder of your own",
            ))?
            .join(CACHE_FOLDER);
        self.open_in(&folder)
    }

    pub fn open_in(&self, dir: &Path) -> Result<Dictionary, Error> {
        Dictionary::open(self.install_in(dir)?)
    }

    pub fn install_in(&self, dir: &Path) -> Result<PathBuf, Error> {
        fs::create_dir_all(dir).map_err(io_error(IoAction::Create, dir))?;
        let path = dir.join(self.file_name);
        if self.is_installed_at(&path) {
            return Ok(path);
        }
        match write_file(&path, |writer| writer.write_all(self.bytes)) {
            Err(Error::Io {
                action: IoAction::Rename,
                ..
            }) if self.is_installed_at(&path) => Ok(path),
            other => other.map(|()| path),
        }
    }

    fn is_installed_at(&self, path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|meta| meta.len() == self.bytes.len() as u64)
            && fs::read(path).is_ok_and(|content| content == self.bytes)
    }
}
