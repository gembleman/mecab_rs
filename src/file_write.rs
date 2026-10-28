use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::error::Error;

const WRITE_BUFFER_BYTES: usize = 1 << 23;

pub fn write_file_with<E: From<Error>>(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<(), E>,
) -> Result<(), E> {
    let io_error = |source| {
        E::from(Error::Io {
            path: path.to_path_buf(),
            source,
        })
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = path.with_file_name(format!(".{name}.tmp"));
    let result = (|| {
        let file = File::create(&temporary).map_err(io_error)?;
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
        write(&mut writer)?;
        writer.flush().map_err(io_error)
    })();
    result
        .and_then(|()| fs::rename(&temporary, path).map_err(io_error))
        .inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
}

pub fn write_file(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> Result<(), Error> {
    write_file_with(path, |writer| {
        write(writer).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    })
}
