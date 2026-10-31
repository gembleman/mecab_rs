use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{Error, IoAction, io_error};
use crate::file_write::temporary_path;
use crate::packed::VERSION as MRD_VERSION;
use crate::wire::{FNV_OFFSET, fnv1a};

const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0];
const IDENTITY_HEAD_BYTES: u64 = 4096;
const IDENTITY_TAIL_BYTES: u64 = 64 << 10;
const DIGEST_HEX_DIGITS: usize = 16;
const STALE_LEFTOVER_AGE: Duration = Duration::from_secs(3600);
const XZ_SUFFIX: &str = ".xz";
const RUNTIME_SUFFIX: &str = ".mrd";
const LEGACY_MARKER_SUFFIX: &str = ".src";
const LOCK_SUFFIX: &str = ".lock";
const VERSION_MARK: &str = ".v";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expansion {
    Reused,
    Expanded,
}

pub(crate) struct Cache {
    pub(crate) path: PathBuf,
    pub(crate) expansion: Expansion,
}

pub(crate) fn is_xz(path: &Path) -> bool {
    let mut magic = [0u8; 6];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == XZ_MAGIC
}

#[derive(Clone, Copy)]
struct TransportIdentity {
    length: u64,
    head_tail_digest: u64,
}

fn identify(xz: &Path) -> Result<TransportIdentity, Error> {
    let mut file = File::open(xz).map_err(io_error(IoAction::Open, xz))?;
    let length = file
        .metadata()
        .map_err(io_error(IoAction::Inspect, xz))?
        .len();
    let mut digest = FNV_OFFSET;
    let mut buffer = Vec::new();
    let head = IDENTITY_HEAD_BYTES.min(length);
    let tail_from = length.saturating_sub(IDENTITY_TAIL_BYTES).max(head);
    for (from, to) in [(0, head), (tail_from, length)] {
        buffer.clear();
        file.seek(SeekFrom::Start(from))
            .map_err(io_error(IoAction::Seek, xz))?;
        (&mut file)
            .take(to - from)
            .read_to_end(&mut buffer)
            .map_err(io_error(IoAction::Read, xz))?;
        digest = fnv1a(digest, &buffer);
    }
    Ok(TransportIdentity {
        length,
        head_tail_digest: digest,
    })
}

fn strip_suffix_ignoring_case<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let at = text.len().checked_sub(suffix.len())?;
    (text.is_char_boundary(at) && text[at..].eq_ignore_ascii_case(suffix)).then(|| &text[..at])
}

fn base_name(xz: &Path) -> String {
    let name = xz.file_name().unwrap_or_default().to_string_lossy();
    let without_xz = strip_suffix_ignoring_case(&name, XZ_SUFFIX).unwrap_or(&name);
    strip_suffix_ignoring_case(without_xz, RUNTIME_SUFFIX)
        .unwrap_or(without_xz)
        .to_owned()
}

fn cache_name(base: &str, identity: TransportIdentity) -> String {
    format!(
        "{base}{VERSION_MARK}{MRD_VERSION}-{:x}-{:0width$x}{RUNTIME_SUFFIX}",
        identity.length,
        identity.head_tail_digest,
        width = DIGEST_HEX_DIGITS
    )
}

pub fn cache_path(xz: &Path) -> Result<PathBuf, Error> {
    let identity = identify(xz)?;
    Ok(xz.with_file_name(cache_name(&base_name(xz), identity)))
}

fn split_prefix(text: &str, accepts: impl Fn(u8) -> bool) -> Option<(&str, &str)> {
    let length = text.bytes().take_while(|&byte| accepts(byte)).count();
    (length > 0).then(|| text.split_at(length))
}

fn split_cache_name<'a>(text: &'a str, base: &str) -> Option<(&'a str, &'a str)> {
    let after_base = text.strip_prefix(base)?.strip_prefix(VERSION_MARK)?;
    let (_, rest) = split_prefix(after_base, |byte| byte.is_ascii_digit())?;
    let rest = rest.strip_prefix('-')?;
    let (_, rest) = split_prefix(rest, |byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))?;
    let rest = rest.strip_prefix('-')?;
    let (digest, rest) = split_prefix(rest, |byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))?;
    let rest = rest.strip_prefix(RUNTIME_SUFFIX)?;
    (digest.len() == DIGEST_HEX_DIGITS).then(|| text.split_at(text.len() - rest.len()))
}

fn is_temporary_remainder(remainder: &str) -> bool {
    let mut parts = remainder.strip_prefix('.').unwrap_or("").splitn(3, '.');
    let is_number = |part: Option<&str>| {
        part.is_some_and(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
    };
    is_number(parts.next()) && is_number(parts.next()) && parts.next().is_some()
}

enum Leftover<'a> {
    Cache(&'a str),
    Lock(&'a str),
    Temporary,
}

fn classify<'a>(entry: &'a str, base: &str) -> Option<Leftover<'a>> {
    if let Some(hidden) = entry.strip_prefix('.') {
        let (_, remainder) = split_cache_name(hidden, base)?;
        return is_temporary_remainder(remainder).then_some(Leftover::Temporary);
    }
    let (name, remainder) = split_cache_name(entry, base)?;
    match remainder {
        "" => Some(Leftover::Cache(name)),
        LOCK_SUFFIX => Some(Leftover::Lock(name)),
        _ => None,
    }
}

fn is_stale(path: &Path, now: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| {
            now.duration_since(modified)
                .is_ok_and(|age| age >= STALE_LEFTOVER_AGE)
        })
}

fn remove_leftover(path: &Path) {
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

fn remove_leftovers(cache: &Path, base: &str) {
    let current = cache.file_name().unwrap_or_default().to_string_lossy();
    let directory = cache
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let Ok(entries) = fs::read_dir(directory.unwrap_or(Path::new("."))) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let removable = match classify(&name, base) {
            Some(Leftover::Cache(found) | Leftover::Lock(found)) => found != current,
            Some(Leftover::Temporary) => is_stale(&path, now),
            None => false,
        };
        if removable {
            remove_leftover(&path);
        }
    }
}

fn remove_legacy_cache(xz: &Path) {
    let name = xz.file_name().unwrap_or_default().to_string_lossy();
    let legacy = match strip_suffix_ignoring_case(&name, XZ_SUFFIX) {
        Some(stem) => xz.with_file_name(stem),
        None => xz.with_file_name(format!("{name}{RUNTIME_SUFFIX}")),
    };
    let mut marker = legacy.clone().into_os_string();
    marker.push(LEGACY_MARKER_SUFFIX);
    let marker = PathBuf::from(marker);
    if marker.is_file() {
        let _ = fs::remove_file(&legacy);
        let _ = fs::remove_file(&marker);
    }
}

pub(crate) fn ensure(xz: &Path) -> Result<Cache, Error> {
    let identity = identify(xz)?;
    let base = base_name(xz);
    let path = xz.with_file_name(cache_name(&base, identity));
    remove_leftovers(&path, &base);
    remove_legacy_cache(xz);
    let expansion = if path.is_file() {
        Expansion::Reused
    } else {
        expand_exclusively(xz, &path)?
    };
    Ok(Cache { path, expansion })
}

pub(crate) fn discard_damaged(cache: &Path) -> Result<(), Error> {
    match fs::remove_file(cache) {
        Err(source) if source.kind() != io::ErrorKind::NotFound => {
            Err(Error::io(IoAction::Remove, cache, source))
        }
        _ => Ok(()),
    }
}

fn lock_path(cache: &Path) -> PathBuf {
    let mut name = cache.file_name().unwrap_or_default().to_os_string();
    name.push(LOCK_SUFFIX);
    cache.with_file_name(name)
}

fn lock_exclusively(lock: &Path) -> Result<File, Error> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock)
        .map_err(io_error(IoAction::Create, lock))?;
    file.lock().map_err(io_error(IoAction::Lock, lock))?;
    Ok(file)
}

fn expand_exclusively(xz: &Path, cache: &Path) -> Result<Expansion, Error> {
    let _held = lock_exclusively(&lock_path(cache))?;
    expand_holding_lock(xz, cache)
}

fn expand_holding_lock(xz: &Path, cache: &Path) -> Result<Expansion, Error> {
    if cache.is_file() {
        return Ok(Expansion::Reused);
    }
    let temporary = temporary_path(cache, "mrd");
    let installed = expand(xz, &temporary)
        .and_then(|()| flush_to_disk(&temporary))
        .and_then(|()| fs::rename(&temporary, cache).map_err(io_error(IoAction::Rename, cache)));
    match installed {
        Ok(()) => Ok(Expansion::Expanded),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn flush_to_disk(file: &Path) -> Result<(), Error> {
    let handle = OpenOptions::new()
        .write(true)
        .open(file)
        .map_err(io_error(IoAction::Open, file))?;
    handle.sync_all().map_err(io_error(IoAction::Sync, file))
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
