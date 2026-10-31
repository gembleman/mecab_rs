use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::error::{Error, IoAction};

const WRITE_BUFFER_BYTES: usize = 1 << 23;

pub fn write_file_with<E: From<Error>>(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<(), E>,
) -> Result<(), E> {
    let io_error = |action, source| E::from(Error::io(action, path, source));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = path.with_file_name(format!(".{name}.tmp"));
    let result = (|| {
        let file = File::create(&temporary).map_err(|source| io_error(IoAction::Create, source))?;
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
        write(&mut writer)?;
        writer
            .flush()
            .map_err(|source| io_error(IoAction::Write, source))
    })();
    result
        .and_then(|()| {
            fs::rename(&temporary, path).map_err(|source| io_error(IoAction::Rename, source))
        })
        .inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
}

pub fn write_file(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> Result<(), Error> {
    write_file_with(path, |writer| {
        write(writer).map_err(|source| Error::io(IoAction::Write, path, source))
    })
}
