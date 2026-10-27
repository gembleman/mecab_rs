
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::error::Error;

const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0];
const HEAD_BYTES: u64 = 4096;
const TAIL_BYTES: u64 = 64 << 10;
const WAIT: Duration = Duration::from_secs(2);

static LOCK: Mutex<()> = Mutex::new(());
static NEXT: AtomicUsize = AtomicUsize::new(0);

fn io(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

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
    name.push(".src");
    cache.with_file_name(name)
}

fn temporary_path(cache: &Path, suffix: &str) -> PathBuf {
    let name = cache.file_name().unwrap_or_default().to_string_lossy();
    cache.with_file_name(format!(
        ".{name}.{}.{}.{suffix}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn fnv1a(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |state, &byte| {
        (state ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

fn identify(xz: &Path) -> Result<(u64, u64), Error> {
    let mut file = File::open(xz).map_err(io(xz))?;
    let length = file.metadata().map_err(io(xz))?.len();
    let mut digest = 0xCBF2_9CE4_8422_2325;
    let mut buffer = Vec::new();
    let head = HEAD_BYTES.min(length);
    let tail_from = length.saturating_sub(TAIL_BYTES).max(head);
    for (from, to) in [(0, head), (tail_from, length)] {
        buffer.clear();
        file.seek(SeekFrom::Start(from)).map_err(io(xz))?;
        (&mut file)
            .take(to - from)
            .read_to_end(&mut buffer)
            .map_err(io(xz))?;
        digest = fnv1a(digest, &buffer);
    }
    Ok((length, digest))
}

fn marker_text(identity: (u64, u64), cache_length: u64) -> String {
    format!(
        "mecab_rs transport cache 1\nsource_length={}\nsource_digest={:016x}\ncache_length={}\n",
        identity.0, identity.1, cache_length
    )
}

fn is_fresh(cache: &Path, marker: &Path, identity: (u64, u64)) -> bool {
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
        .map_err(io(&temporary))
        .and_then(|()| fs::rename(&temporary, path).map_err(io(path)));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn ensure(xz: &Path, force: bool) -> Result<(PathBuf, bool), Error> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let cache = cache_path(xz);
    let marker = marker_path(&cache);
    let identity = identify(xz)?;
    if !force && is_fresh(&cache, &marker, identity) {
        return Ok((cache, false));
    }
    let _ = fs::remove_file(&marker);
    let temporary = temporary_path(&cache, "mrd");
    let expanded = expand(xz, &temporary).and_then(|()| {
        OpenOptions::new()
            .write(true)
            .open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(io(&temporary))?;
        Ok(fs::metadata(&temporary).map_err(io(&temporary))?.len())
    });
    let length = match expanded {
        Ok(length) => length,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = fs::rename(&temporary, &cache).map_err(io(&cache)) {
        let _ = fs::remove_file(&temporary);
        let started = Instant::now();
        while started.elapsed() < WAIT {
            if is_fresh(&cache, &marker, identity) {
                return Ok((cache, false));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        return Err(error);
    }
    let text = marker_text(identity, length);
    if let Err(error) = write_replacing(&marker, text.as_bytes())
        && !is_fresh(&cache, &marker, identity)
    {
        return Err(error);
    }
    Ok((cache, true))
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
