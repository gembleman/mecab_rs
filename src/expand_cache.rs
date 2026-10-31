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
const BAD_SUFFIX: &str = "bad";
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
    judged: Option<Fingerprint>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    length: u64,
    modified: SystemTime,
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = fs::metadata(path).ok()?;
    Some(Fingerprint {
        length: metadata.len(),
        modified: metadata.modified().ok()?,
    })
}

fn cache_at(path: PathBuf, expansion: Expansion) -> Cache {
    let judged = fingerprint(&path);
    Cache {
        path,
        expansion,
        judged,
    }
}

pub(crate) fn ensure(xz: &Path) -> Result<Cache, Error> {
    let identity = identify(xz)?;
    let path = xz.with_file_name(cache_name(&base_name(xz), identity));
    if path.is_file() {
        return Ok(cache_at(path, Expansion::Reused));
    }
    let _held = lock_exclusively(&lock_path(&path))?;
    let expansion = expand_holding_lock(xz, &path)?;
    if expansion == Expansion::Expanded {
        clean_around(xz, &path);
    }
    Ok(cache_at(path, expansion))
}

pub(crate) fn replace_damaged(xz: &Path, damaged: &Cache) -> Result<Cache, Error> {
    let _held = lock_exclusively(&lock_path(&damaged.path))?;
    let still_the_judged_file =
        damaged.judged.is_some() && fingerprint(&damaged.path) == damaged.judged;
    if still_the_judged_file {
        quarantine(&damaged.path)?;
    }
    let expansion = expand_holding_lock(xz, &damaged.path)?;
    if expansion == Expansion::Expanded {
        clean_around(xz, &damaged.path);
    }
    Ok(cache_at(damaged.path.clone(), expansion))
}

fn quarantine(cache: &Path) -> Result<(), Error> {
    let quarantined = temporary_path(cache, BAD_SUFFIX);
    match fs::rename(cache, &quarantined) {
        Ok(()) => {
            let _ = fs::remove_file(&quarantined);
            Ok(())
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io(IoAction::Rename, cache, source)),
    }
}

fn clean_around(xz: &Path, cache: &Path) {
    remove_leftovers(cache, &base_name(xz));
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

fn expand_holding_lock(xz: &Path, cache: &Path) -> Result<Expansion, Error> {
    if cache.is_file() {
        return Ok(Expansion::Reused);
    }
    expand(xz, cache).map(|()| Expansion::Expanded)
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
