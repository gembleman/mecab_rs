
use std::error::Error;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use lindera::dictionary::{DictionaryBuilder, Metadata};
use vibrato::dictionary::SystemDictionaryBuilder;

fn build_vibrato(src: &Path, lex: impl std::io::Read, out: &str) -> Result<(), Box<dyn Error>> {
    let t = Instant::now();
    let dict = SystemDictionaryBuilder::from_readers(
        lex,
        BufReader::with_capacity(1 << 20, File::open(src.join("matrix.def"))?),
        File::open(src.join("char.def"))?,
        File::open(src.join("unk.def"))?,
    )?;
    eprintln!("vibrato: compiled in {:.1}s", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let mut f = zstd::Encoder::new(File::create(out)?, 19)?;
    dict.write(&mut f)?;
    f.finish()?;
    eprintln!("vibrato: written (zstd 19) in {:.1}s", t.elapsed().as_secs_f64());
    Ok(())
}

fn build_lindera(src: &Path, metadata: &str, out: &str) -> Result<(), Box<dyn Error>> {
    let metadata: Metadata = serde_json::from_reader(File::open(metadata)?)?;
    let t = Instant::now();
    DictionaryBuilder::new(metadata).build_dictionary(src, &PathBuf::from(out))?;
    eprintln!("lindera: built in {:.1}s", t.elapsed().as_secs_f64());
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let a: Vec<String> = std::env::args().collect();
    match (a.get(1).map(String::as_str), a.len()) {
        (Some("vibrato"), 4) => {
            let src = PathBuf::from(&a[2]);
            return build_vibrato(&src, File::open(src.join("lex.csv"))?, &a[3]);
        }
        (Some("lindera"), 5) => return build_lindera(&PathBuf::from(&a[2]), &a[3], &a[4]),
        (_, 5) => {}
        _ => {
            eprintln!("usage: build-dicts <utf8-ipadic-dir> <lindera-metadata.json> <out-vibrato.zst> <out-lindera-dir>");
            eprintln!("       build-dicts vibrato <src-dir> <out-vibrato.zst>");
            eprintln!("       build-dicts lindera <src-dir> <lindera-metadata.json> <out-lindera-dir>");
            std::process::exit(2);
        }
    }
    let src = PathBuf::from(&a[1]);

    let mut csvs: Vec<PathBuf> = fs::read_dir(&src)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map_or(false, |x| x == "csv"))
        .collect();
    csvs.sort();
    let mut lex = Vec::new();
    for p in &csvs {
        lex.extend(fs::read(p)?);
    }
    build_vibrato(&src, lex.as_slice(), &a[3])?;
    build_lindera(&src, &a[2], &a[4])
}
