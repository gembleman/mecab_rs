
use std::error::Error;
use std::fs::{self, File};
use std::path::PathBuf;
use std::time::Instant;

use lindera::dictionary::{DictionaryBuilder, Metadata};
use vibrato::dictionary::SystemDictionaryBuilder;

fn main() -> Result<(), Box<dyn Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 5 {
        eprintln!("usage: build-dicts <utf8-ipadic-dir> <lindera-metadata.json> <out-vibrato.zst> <out-lindera-dir>");
        std::process::exit(2);
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
    let t = Instant::now();
    let dict = SystemDictionaryBuilder::from_readers(
        lex.as_slice(),
        File::open(src.join("matrix.def"))?,
        File::open(src.join("char.def"))?,
        File::open(src.join("unk.def"))?,
    )?;
    eprintln!("vibrato: compiled in {:.1}s", t.elapsed().as_secs_f64());
    let mut f = zstd::Encoder::new(File::create(&a[3])?, 19)?;
    dict.write(&mut f)?;
    f.finish()?;

    let metadata: Metadata = serde_json::from_reader(File::open(&a[2])?)?;
    let t = Instant::now();
    DictionaryBuilder::new(metadata).build_dictionary(&src, &PathBuf::from(&a[4]))?;
    eprintln!("lindera: built in {:.1}s", t.elapsed().as_secs_f64());
    Ok(())
}
