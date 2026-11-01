use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{Error, IoAction};

const WRITE_BUFFER_BYTES: usize = 1 << 20;

static NEXT_TEMPORARY_NUMBER: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn temporary_path(target: &Path, suffix: &str) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(
        ".{name}.{}.{}.{suffix}",
        std::process::id(),
        NEXT_TEMPORARY_NUMBER.fetch_add(1, Ordering::Relaxed)
    ))
}

pub fn write_file_with<E: From<Error>>(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<(), E>,
) -> Result<(), E> {
    let io_error = |action, source| E::from(Error::io(action, path, source));
    let temporary = temporary_path(path, "tmp");
    let result = (|| {
        let file = File::create(&temporary).map_err(|source| io_error(IoAction::Create, source))?;
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
        write(&mut writer)?;
        let file = writer
            .into_inner()
            .map_err(|failure| io_error(IoAction::Write, failure.into_error()))?;
        file.sync_all()
            .map_err(|source| io_error(IoAction::Sync, source))
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
