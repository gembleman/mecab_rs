# mecab_rs

English | [日本語](https://github.com/gembleman/mecab_rs/blob/main/README.ja.md)

[![crates.io](https://img.shields.io/crates/v/mecab_rs.svg)](https://crates.io/crates/mecab_rs)
[![docs.rs](https://img.shields.io/docsrs/mecab_rs)](https://docs.rs/mecab_rs)
[![license](https://img.shields.io/crates/l/mecab_rs.svg)](https://github.com/gembleman/mecab_rs/blob/main/LICENSE)

**Japanese morphological analysis in pure Rust: MeCab's exact output, faster and in a fraction of
the memory.**

- **Same output as MeCab.** A line-by-line port of MeCab 0.996: for the same dictionary the best
  path is byte-identical to libmecab's (surfaces, features, costs), checked on about 600,000
  UniDic sentences. No libmecab, no C code.
- **Fast.** On UniDic, 67,000 sentences/s on one thread: 1.9 times vibrato, 2.4 times lindera,
  2.7 times sudachi.rs ([benchmarks](#benchmarks)).
- **Small in memory.** 92 MB peak working set with full UniDic (vibrato 1.3 GB, lindera 569 MB),
  14 MB of it private.
- **Tiny to ship.** The whole UniDic is a 6.4 MB `.mrd.xz` (the same UniDic is 407 MB for vibrato
  even with zstd -19, 1.3 GB for lindera), small enough to come from crates.io as a normal
  dependency; it is expanded once on the first open.
- **Batteries included.** `features = ["unidic"]` and one line give a ready tagger, plus N-best,
  constraints, marginals, user dictionaries and ports of MeCab's dictionary and training tools.

```toml
mecab_rs = { version = "0.1", features = ["unidic"] }
```

```rust,ignore
use mecab_rs::{bundled, NodeKind, Tagger};

let dictionary = bundled::UNIDIC.open()?;
let mut tagger = Tagger::new(&dictionary);
for node in tagger.parse("吾輩は猫である")? {
    if !matches!(node.kind(), NodeKind::Bos | NodeKind::Eos) {
        println!("{}\t{}", node.surface(), node.feature());
    }
}
```

```text
吾輩	代名詞,*,*,*,*,*,ワガハイ,我が輩,吾輩,ワガハイ,吾輩,ワガハイ,混,...
は	助詞,係助詞,*,*,*,*,ハ,は,は,ワ,は,ワ,和,...
猫	名詞,普通名詞,一般,*,*,*,ネコ,猫,猫,ネコ,猫,ネコ,和,...
で	助動詞,*,*,*,助動詞-ダ,連用形-一般,ダ,だ,で,デ,だ,ダ,和,...
ある	動詞,非自立可能,*,*,五段-ラ行,終止形-一般,アル,有る,ある,アル,ある,アル,和,...
```

## Overview

Dictionaries are read from a single `.mrd` runtime file, produced from a compiled MeCab dictionary
directory by `mecab-rs-migrate pack`. For distribution the file can be compressed as `.mrd.xz`:
`Dictionary::open` accepts it, expands it next to itself on the first open, and memory-maps the
expanded file from then on.

## Features

- Dictionary lookup (double array), unknown-word generation with MeCab's character categories,
  one-best Viterbi search.
- Boundary and feature constraints and MeCab's partial-mode (`-p`) text format (`PartialInput`).
- N-best paths (`Tagger::parse_nbest`), all lattice nodes like `-a` (`Tagger::parse_lattice`) and
  marginal probabilities like `-m -t` (`Tagger::parse_marginal`).
- User dictionaries (`Dictionary::open_with_user`, up to 255).
- Characters outside the BMP are classified (ideographs of planes 2 and 3, kana supplements,
  variation selectors), where libmecab puts them all in the category of U+0000.
- Dictionary building, CRF cost training and packing tools, ports of `mecab-dict-index`,
  `mecab-cost-train`, `mecab-dict-gen`, `mecab-system-eval` and `mecab-test-gen`.
- Dependencies: `memmap2`, plus `lzma-rust2` (decoder only) for `.mrd.xz` in the default `xz`
  feature. Little-endian targets only; dictionaries must be MeCab version 102, UTF-8.

## Installation

```toml
[dependencies]
mecab_rs = "0.1"
```

## Bundled dictionaries

The `unidic` and `ipadic` features bundle a dictionary, like rusqlite's `bundled`. Each pulls in a
data-only crate holding the dictionary as `.mrd.xz` (UniDic 6.4 MB, IPAdic 2.1 MB):

```toml
mecab_rs = { version = "0.1", features = ["unidic"] }
```

```rust,ignore
let dictionary = mecab_rs::bundled::UNIDIC.open()?;
let dictionary = mecab_rs::bundled::IPADIC.open_in(dir)?;
```

`open` uses the `mecab_rs` folder inside the platform cache folder (`dirs::cache_dir`); `open_in`
uses the folder you give. The `.mrd.xz` is written there once and expanded next to itself on the
first open (UniDic about 6 s and about 500 MiB peak working set, IPAdic about 1.2 s); later opens
map the expanded file at once.

## Usage

```rust,no_run
use std::sync::Arc;

use mecab_rs::{BoundaryKind, Constraints, Dictionary, NodeKind, Tagger};

fn main() -> Result<(), mecab_rs::Error> {
    let dictionary = Arc::new(Dictionary::open("path/to/dictionary.mrd")?);
    let mut tagger = Tagger::new(Arc::clone(&dictionary));

    for node in tagger.parse("今日はいい天気ですね")? {
        if node.kind() == NodeKind::Normal || node.kind() == NodeKind::Unknown {
            println!("{}\t{}", node.surface(), node.feature());
        }
    }

    let mut constraints = Constraints::new("今日はいい天気ですね");
    constraints.boundary(6, BoundaryKind::Token)?;
    let constrained = tagger.parse_with(&constraints)?;
    println!("{} nodes", constrained.len());
    Ok(())
}
```

`Dictionary` is `Send + Sync`; give each thread its own `Tagger`. `Tagger::new` takes an
`Arc<Dictionary>` or a `&Dictionary`. Paths start with a BOS node and end with an EOS node, as in
MeCab. Positions in constraints are byte offsets into the text.

N-best paths, lattice views and marginals:

```rust,ignore
let mut nbest = tagger.parse_nbest(&Constraints::new("今日はいい天気ですね"))?;
while let Some(path) = nbest.next_path() {
    println!("{} nodes", path.len());
}

let view = tagger.parse_marginal(&Constraints::new("今日はいい天気ですね"), 0.003)?;
for node in view.best_path() {
    println!("{}\t{:?}", node.surface(), view.prob(node));
}
```

## Dictionary files

`Dictionary::open` reads one `.mrd` file (or `.mrd.xz`). It does not open a MeCab directory.
Convert a compiled directory (which needs a `char.uni`, see the next section) with:

```text
mecab-rs-migrate pack path/to/dictionary path/to/dictionary.mrd
```

The expanded cache of an `.mrd.xz` is written next to it under a temporary name and renamed, with
a lock file so that concurrent opens wait for one expansion; the folder must be writable on the
first open.

`libmecab` reads `char.bin`, a table for the BMP. This crate reads `char.uni`, a table for all
code points packed into the `.mrd`. `mecab-rs-migrate path/to/dictionary` adds `char.uni` to a
MeCab directory (the BMP part is a copy of `char.bin`; ranges above U+FFFF in `char.def` are
applied if present).

## Building a dictionary

```text
cargo build --release -p mecab_rs_tools
```

`mecab_rs_tools` has six binaries:

| Binary | Port of | Purpose |
|---|---|---|
| `mecab-rs-dict-index` | `mecab-dict-index` | compile dictionary sources and user dictionaries |
| `mecab-rs-cost-train` | `mecab-cost-train` | CRF cost training (L-BFGS) |
| `mecab-rs-dict-gen` | `mecab-dict-gen` | write dictionary sources from a seed dictionary and a model |
| `mecab-rs-system-eval` | `mecab-system-eval` | evaluate analysis against an answer file |
| `mecab-rs-test-gen` | `mecab-test-gen` | make test sentences from an answer file |
| `mecab-rs-migrate` | (new) | `pack`, `rebuild`, and `char.uni` migration |

`mecab-rs-dict-index` takes the options of the C tool and writes files byte-identical to it:

```text
mecab-rs-dict-index -d path/to/sources -o path/to/out
mecab-rs-dict-index -d path/to/sources -u user.dic user.csv [more.csv ...]
mecab-rs-dict-index -d path/to/sources -u assigned.csv -a -m model.bin user.csv
```

Only UTF-8 is supported. The training pipeline:

```text
mecab-rs-cost-train -d seed corpus.txt model
mecab-rs-dict-gen -d seed -o new-sources -m model [-b]
mecab-rs-dict-index -d new-sources -o new-dictionary
```

`mecab-rs-migrate rebuild` turns a full compiled UniDic directory into a much smaller runtime
directory (ids renumbered by frequency, integer connection templates, optional feature slimming
with `--keep`), which `mecab-rs-migrate pack` then packs:

```text
mecab-rs-migrate rebuild path/to/unidic-full path/to/runtime --corpus sentences.txt --keep 0-7,9,10,12,20,22
mecab-rs-migrate pack path/to/runtime path/to/runtime.mrd
```

The same steps are available as the `mecab_rs_tools::build`, `learn` and `rebuild` modules.

## Compatibility with libmecab

- With UniDic-CWJ 2025.12, the best path is identical to libmecab's over about 600,000 sentences,
  including constrained parses. Ties in cost go to the later candidate, as in libmecab.
- Outside the BMP the character classification differs on purpose (see Features).
- When a text ends with whitespace, libmecab adds a dead-end unknown node past the end of the
  text. This crate does not. Best and N-best paths are the same; `parse_lattice` lists one node
  less and marginals of the nodes before it differ by about 1e-6.
- N-best paths have the same costs at every rank, but paths of equal cost may come in another order.
- None of libmecab's C buffer limits apply (hits per position, node and text length).
- Not implemented: output formats and other MeCab options such as soft-wakati.

## Benchmarks

`compare/` is the benchmark harness used for the numbers below: one thread, UniDic-CWJ 2025.12
for mecab_rs, vibrato and lindera (sudachi.rs cannot read it and uses SudachiDict core), 91,000
sentences of Aozora Bunko works, Ryzen 9 3900X, Windows 10, 3 warm-up and 9 timed passes, medians.
Compared with [vibrato](https://github.com/daac-tools/vibrato) 0.5.2,
[lindera](https://github.com/lindera/lindera) and
[sudachi.rs](https://github.com/WorksApplications/sudachi.rs).

| | dict on disk | load | peak WS | peak private | sentences/s, surfaces | sentences/s, with features |
|---|---:|---:|---:|---:|---:|---:|
| mecab_rs | 84.4 MB (`.mrd`), 6.39 MB (`.mrd.xz`) | 24 ms | 92.0 MB | 14.4 MB | 67,293 | 55,090 |
| vibrato | 407.2 MB | 13,558 ms | 1299 MB | 1305 MB | 34,999 | 33,463 |
| lindera | 1,305.3 MB | 3 ms | 569 MB | 19 MB | 28,189 | 19,903 |
| sudachi.rs | 202.2 MB | 33 ms | 163 MB | 17 MB | 25,187 | 27,634 |

The mapped pages of the `.mrd` count in the working set (WS) but are shared file cache, not private
memory. On mecab-ipadic 2.7.0 mecab_rs is about 1.17 times faster than vibrato on surfaces, but with
features read vibrato is about 1.04 times faster (224,443 / 173,867 sentences/s for mecab_rs,
191,165 / 180,501 for vibrato).
mecab_rs's segmentation equals vibrato's on every sentence and lindera's on 99.8%.

## License

BSD-3-Clause. This crate is a line-by-line port of MeCab, which is available under the BSD
license; the MeCab copyright notice is kept first in `LICENSE`.
