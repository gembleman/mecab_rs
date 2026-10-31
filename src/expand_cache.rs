use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::error::{Error, io_error};
use crate::packed::VERSION as MRD_VERSION;
use crate::wire::{FNV_OFFSET, fnv1a};

const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0];
const IDENTITY_HEAD_BYTES: u64 = 4096;
const IDENTITY_TAIL_BYTES: u64 = 64 << 10;
const CONCURRENT_WRITER_WAIT: Duration = Duration::from_secs(2);
const STALE_TEMPORARY_AGE: Duration = Duration::from_secs(3600);
const MARKER_SUFFIX: &str = ".src";

static EXPAND_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TEMPORARY_NUMBER: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn is_xz(path: &Path) -> bool {
    let mut magic = [0u8; 6];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == XZ_MAGIC
}

pub(crate) fn cache_path(xz: &Path) -> PathBuf {
    let name = xz.file_name().unwrap_or_default().to_string_lossy();
    let stem = match name.len().checked_sub(3) {
        Some(at) if name.is_char_boundary(at) && name[at..].eq_ignore_ascii_case(".xz") => {
            name[..at].to_owned()
        }
        _ => format!("{name}.mrd"),
    };
    xz.with_file_name(stem)
}

fn marker_path(cache: &Path) -> PathBuf {
    let mut name = cache.file_name().unwrap_or_default().to_os_string();
    name.push(MARKER_SUFFIX);
    cache.with_file_name(name)
}

fn temporary_path(cache: &Path, suffix: &str) -> PathBuf {
    let name = cache.file_name().unwrap_or_default().to_string_lossy();
    cache.with_file_name(format!(
        ".{name}.{}.{}.{suffix}",
        std::process::id(),
        NEXT_TEMPORARY_NUMBER.fetch_add(1, Ordering::Relaxed)
    ))
}

const MARKER_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
struct TransportIdentity {
    length: u64,
    head_tail_digest: u64,
}

fn identify(xz: &Path) -> Result<TransportIdentity, Error> {
    let mut file = File::open(xz).map_err(io_error(xz))?;
    let length = file.metadata().map_err(io_error(xz))?.len();
    let mut digest = FNV_OFFSET;
    let mut buffer = Vec::new();
    let head = IDENTITY_HEAD_BYTES.min(length);
    let tail_from = length.saturating_sub(IDENTITY_TAIL_BYTES).max(head);
    for (from, to) in [(0, head), (tail_from, length)] {
        buffer.clear();
        file.seek(SeekFrom::Start(from)).map_err(io_error(xz))?;
        (&mut file)
            .take(to - from)
            .read_to_end(&mut buffer)
            .map_err(io_error(xz))?;
        digest = fnv1a(digest, &buffer);
    }
    Ok(TransportIdentity {
        length,
        head_tail_digest: digest,
    })
}

fn marker_text(identity: TransportIdentity, cache_length: u64) -> String {
    format!(
        "mecab_rs transport cache 1\nmrd_version={}\nsource_length={}\nsource_digest={:016x}\ncache_length={}\n",
        MRD_VERSION, identity.length, identity.head_tail_digest, cache_length
    )
}

fn is_fresh(cache: &Path, marker: &Path, identity: TransportIdentity) -> bool {
    let (Ok(text), Ok(metadata)) = (fs::read_to_string(marker), fs::metadata(cache)) else {
        return false;
    };
    text == marker_text(identity, metadata.len())
}

fn write_replacing(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let temporary = temporary_path(path, "tmp");
    let result = File::create(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .map_err(io_error(&temporary))
        .and_then(|()| fs::rename(&temporary, path).map_err(io_error(path)));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn flush_to_disk(file: &Path) -> Result<u64, Error> {
    OpenOptions::new()
        .write(true)
        .open(file)
        .and_then(|handle| handle.sync_all())
        .map_err(io_error(file))?;
    Ok(fs::metadata(file).map_err(io_error(file))?.len())
}

fn wait_until_fresh(cache: &Path, marker: &Path, identity: TransportIdentity) -> bool {
    let started = Instant::now();
    while started.elapsed() < CONCURRENT_WRITER_WAIT {
        if is_fresh(cache, marker, identity) {
            return true;
        }
        std::thread::sleep(MARKER_POLL);
    }
    false
}

pub(crate) fn ensure(xz: &Path, force: bool) -> Result<(PathBuf, bool), Error> {
    let _guard = EXPAND_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = cache_path(xz);
    let marker = marker_path(&cache);
    let identity = identify(xz)?;
    if !force && is_fresh(&cache, &marker, identity) {
        return Ok((cache, false));
    }
    remove_stale_temporaries(&cache);
    if force {
        let _ = fs::remove_file(&marker);
    }
    let temporary = temporary_path(&cache, "mrd");
    let expanded = expand(xz, &temporary).and_then(|()| flush_to_disk(&temporary));
    let length = match expanded {
        Ok(length) => length,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    let installed = install(&temporary, &cache, &marker, identity, length)?;
    Ok((cache, installed))
}

fn install(
    temporary: &Path,
    cache: &Path,
    marker: &Path,
    identity: TransportIdentity,
    length: u64,
) -> Result<bool, Error> {
    if let Err(error) = fs::rename(temporary, cache).map_err(io_error(cache)) {
        let _ = fs::remove_file(temporary);
        if wait_until_fresh(cache, marker, identity) {
            return Ok(false);
        }
        return Err(error);
    }
    let text = marker_text(identity, length);
    if let Err(error) = write_replacing(marker, text.as_bytes())
        && !is_fresh(cache, marker, identity)
    {
        return Err(error);
    }
    Ok(true)
}

fn is_temporary_of(entry: &str, cache_name: &str) -> bool {
    let after_name = entry
        .strip_prefix("..")
        .or_else(|| entry.strip_prefix('.'))
        .and_then(|rest| rest.strip_prefix(cache_name));
    let Some(rest) = after_name else {
        return false;
    };
    let rest = rest.strip_prefix(MARKER_SUFFIX).unwrap_or(rest);
    let mut parts = rest.strip_prefix('.').unwrap_or("").splitn(3, '.');
    let is_number = |part: Option<&str>| {
        part.is_some_and(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
    };
    is_number(parts.next()) && is_number(parts.next()) && parts.next().is_some()
}

fn is_stale(path: &Path, now: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| {
            now.duration_since(modified)
                .is_ok_and(|age| age >= STALE_TEMPORARY_AGE)
        })
}

fn remove_stale_temporaries(cache: &Path) {
    let name = cache.file_name().unwrap_or_default().to_string_lossy();
    let directory = cache
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let Ok(entries) = fs::read_dir(directory.unwrap_or(Path::new("."))) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_temporary_of(&entry.file_name().to_string_lossy(), &name) || !is_stale(&path, now) {
            continue;
        }
        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(feature = "xz")]
fn expand(xz: &Path, out: &Path) -> Result<(), Error> {
    crate::packed::expand(xz, out)
}

#[cfg(not(feature = "xz"))]
fn expand(_xz: &Path, _out: &Path) -> Result<(), Error> {
    Err(Error::Unsupported(
        "this is an xz transport file; build mecab_rs with the `xz` feature to open it",
    ))
}
