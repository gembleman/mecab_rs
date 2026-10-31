use std::io::Write;
use std::path::Path;

use lzma_rust2::{XzOptions, XzWriter};

use crate::tool_error::rebuild_error;
use mecab_rs::__private::files::write_file;
use mecab_rs::__private::support::transport_payload;
use mecab_rs::Error;

const MAX_DICT_SIZE: u32 = 512 << 20;
const XZ_PRESET: u32 = 9;
const XZ_NICE_LEN: u32 = 273;
const XZ_DEPTH_LIMIT: i32 = 512;

pub fn compress(payload: &[u8], path: &Path) -> Result<Vec<u8>, Error> {
    let mut options = XzOptions::with_preset(XZ_PRESET);
    options.lzma_options.nice_len = XZ_NICE_LEN;
    options.lzma_options.depth_limit = XZ_DEPTH_LIMIT;
    options.lzma_options.dict_size = options
        .lzma_options
        .dict_size
        .max(u32::try_from(payload.len()).unwrap_or(u32::MAX))
        .min(MAX_DICT_SIZE);
    let failed = |error: std::io::Error| rebuild_error(path, format!("xz: {error}"));
    let mut writer = XzWriter::new(Vec::new(), options).map_err(failed)?;
    writer.write_all(payload).map_err(failed)?;
    writer.finish().map_err(failed)
}

pub fn write_transport(dict_dir: &Path, out_xz: &Path) -> Result<u64, Error> {
    let payload = transport_payload(dict_dir)?;
    let compressed = compress(&payload, out_xz)?;
    write_file(out_xz, |writer| writer.write_all(&compressed))?;
    Ok(compressed.len() as u64)
}
