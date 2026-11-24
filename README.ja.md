# mecab_rs

[English](https://github.com/gembleman/mecab_rs/blob/main/README.md) | 日本語

[![crates.io](https://img.shields.io/crates/v/mecab_rs.svg)](https://crates.io/crates/mecab_rs)
[![docs.rs](https://img.shields.io/docsrs/mecab_rs)](https://docs.rs/mecab_rs)
[![license](https://img.shields.io/crates/l/mecab_rs.svg)](https://github.com/gembleman/mecab_rs/blob/main/LICENSE)

**純 Rust による日本語形態素解析: MeCab と完全に同じ出力を、より高速に、わずかなメモリで。**

- **MeCab と同じ出力。** MeCab 0.996 を一行ずつ移植しています。同じ辞書であれば、最適パスは libmecab と
  バイト単位で一致します(表層形、素性、コスト)。UniDic で約 60 万文を検証済みです。libmecab も C コードも
  不要です。
- **高速。** UniDic で 1 スレッドあたり毎秒 67,000 文。vibrato の 1.9 倍、lindera の 2.4 倍、
  sudachi.rs の 2.7 倍です([ベンチマーク](#ベンチマーク))。
- **小さい。** UniDic をすべて読み込んでもピークのワーキングセットは 92 MB(vibrato は 1.3 GB、lindera は
  569 MB)で、そのうちプライベートは 14 MB です。UniDic 全体を 6.4 MB の `.mrd.xz` として配布できます。
- **すぐに使える。** `features = ["unidic"]` と 1 行だけで使えるタガーが手に入るほか、N-best、制約、
  周辺確率、ユーザー辞書、そして MeCab の辞書ツールおよび学習ツールの移植版も揃っています。

```toml
mecab_rs = { version = "0.1", features = ["unidic"] }
```

```rust
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

## 概要

辞書は単一の `.mrd` ランタイムファイルから読み込みます。このファイルは、コンパイル済みの MeCab 辞書
ディレクトリから `mecab-rs-migrate pack` で作成します。配布用には `.mrd.xz` として圧縮できます。
`Dictionary::open` はこれを受け付け、最初に開いたときに隣へ展開し、以降は展開後のファイルをメモリマップします。

## 機能

- 辞書引き(ダブル配列)、MeCab の文字カテゴリによる未知語生成、1-best の Viterbi 探索。
- 境界制約と素性制約、および MeCab の部分解析モード(`-p`)のテキスト形式(`PartialInput`)。
- N-best パス(`Tagger::parse_nbest`)、`-a` と同様の全ラティスノード(`Tagger::parse_lattice`)、
  `-m -t` と同様の周辺確率(`Tagger::parse_marginal`)。
- ユーザー辞書(`Dictionary::open_with_user`、最大 255 個)。
- BMP 外の文字も分類します(第 2・第 3 面の漢字、仮名の補助ブロック、異体字セレクタ)。libmecab ではこれらがすべて
  U+0000 のカテゴリに入ります。
- 辞書の構築、CRF コスト学習、パッキングの各ツール。`mecab-dict-index`、`mecab-cost-train`、
  `mecab-dict-gen`、`mecab-system-eval`、`mecab-test-gen` の移植版です。
- 依存クレート: `memmap2`、および既定の `xz` フィーチャーで `.mrd.xz` を扱うための `lzma-rust2`(デコーダーのみ)。
  対応はリトルエンディアンのターゲットのみで、辞書は MeCab バージョン 102、UTF-8 である必要があります。

## インストール

```toml
[dependencies]
mecab_rs = "0.1"
```

## 同梱辞書

`unidic` フィーチャーと `ipadic` フィーチャーは、rusqlite の `bundled` と同様に辞書を同梱します。
それぞれ、辞書を `.mrd.xz` として収めたデータ専用クレートを取り込みます(UniDic 6.4 MB、IPAdic 2.1 MB)。

```toml
mecab_rs = { version = "0.1", features = ["unidic"] }
```

```rust
let dictionary = mecab_rs::bundled::UNIDIC.open()?;
let dictionary = mecab_rs::bundled::IPADIC.open_in(dir)?;
```

`open` はプラットフォームのキャッシュフォルダ(`dirs::cache_dir`)内の `mecab_rs` フォルダを使い、
`open_in` は指定したフォルダを使います。`.mrd.xz` はそこに一度だけ書き出され、最初に開いたときに隣へ展開されます
(UniDic は約 6 秒でピークのワーキングセットは約 500 MiB、IPAdic は約 1.2 秒)。2 回目以降は、
展開済みのファイルを即座にマップします。

## 使い方

```rust
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

`Dictionary` は `Send + Sync` です。スレッドごとに専用の `Tagger` を用意してください。`Tagger::new` は
`Arc<Dictionary>` または `&Dictionary` を受け取ります。パスは MeCab と同様に BOS ノードで始まり、EOS ノードで
終わります。制約内の位置は、テキストに対するバイトオフセットです。

N-best パス、ラティスビュー、周辺確率:

```rust
let mut nbest = tagger.parse_nbest(&Constraints::new("今日はいい天気ですね"))?;
while let Some(path) = nbest.next_path() {
    println!("{} nodes", path.len());
}

let view = tagger.parse_marginal(&Constraints::new("今日はいい天気ですね"), 0.003)?;
for node in view.best_path() {
    println!("{}\t{:?}", node.surface(), view.prob(node));
}
```

## 辞書ファイル

`Dictionary::open` は 1 つの `.mrd` ファイル(または `.mrd.xz`)を読み込みます。MeCab のディレクトリは開きません。
コンパイル済みディレクトリ(`char.uni` が必要です。次の節を参照)は、次のコマンドで変換します。

```text
mecab-rs-migrate pack path/to/dictionary path/to/dictionary.mrd
```

`.mrd.xz` の展開キャッシュは、隣に一時的な名前で書き出されたあと改名されます。ロックファイルにより、同時に開いた
場合も 1 回の展開を待つようになっています。最初に開くときは、そのフォルダが書き込み可能である必要があります。

`libmecab` は BMP 用のテーブルである `char.bin` を読み込みます。このクレートは、全コードポイント用のテーブルで
`.mrd` に格納される `char.uni` を読み込みます。`mecab-rs-migrate path/to/dictionary` は MeCab のディレクトリに
`char.uni` を追加します(BMP 部分は `char.bin` のコピーで、`char.def` に U+FFFF を超える範囲があればそれも
適用されます)。

## 辞書の構築

```text
cargo build --release -p mecab_rs_tools
```

`mecab_rs_tools` には 6 つのバイナリがあります。

| バイナリ | 移植元 | 用途 |
|---|---|---|
| `mecab-rs-dict-index` | `mecab-dict-index` | 辞書ソースとユーザー辞書をコンパイルする |
| `mecab-rs-cost-train` | `mecab-cost-train` | CRF コスト学習(L-BFGS) |
| `mecab-rs-dict-gen` | `mecab-dict-gen` | シード辞書とモデルから辞書ソースを書き出す |
| `mecab-rs-system-eval` | `mecab-system-eval` | 正解ファイルに対して解析結果を評価する |
| `mecab-rs-test-gen` | `mecab-test-gen` | 正解ファイルからテスト文を作る |
| `mecab-rs-migrate` | (新規) | `pack`、`rebuild`、`char.uni` の移行 |

`mecab-rs-dict-index` は C 版ツールのオプションをそのまま受け付け、C 版とバイト単位で同一のファイルを書き出します。

```text
mecab-rs-dict-index -d path/to/sources -o path/to/out
mecab-rs-dict-index -d path/to/sources -u user.dic user.csv [more.csv ...]
mecab-rs-dict-index -d path/to/sources -u assigned.csv -a -m model.bin user.csv
```

対応する文字コードは UTF-8 のみです。学習パイプライン:

```text
mecab-rs-cost-train -d seed corpus.txt model
mecab-rs-dict-gen -d seed -o new-sources -m model [-b]
mecab-rs-dict-index -d new-sources -o new-dictionary
```

`mecab-rs-migrate rebuild` は、コンパイル済みの完全な UniDic ディレクトリを、はるかに小さなランタイム用
ディレクトリに変換します(ID を頻度順に振り直し、接続コストを整数テンプレート化し、`--keep` で任意に素性を
絞り込めます)。これを `mecab-rs-migrate pack` でパックします。

```text
mecab-rs-migrate rebuild path/to/unidic-full path/to/runtime --corpus sentences.txt --keep 0-7,9,10,12,20,22
mecab-rs-migrate pack path/to/runtime path/to/runtime.mrd
```

同じ手順は、`mecab_rs_tools::build`、`learn`、`rebuild` の各モジュールとしても利用できます。

## libmecab との互換性

- UniDic-CWJ 2025.12 では、制約付き解析を含め、約 60 万文にわたって最適パスが libmecab と一致します。
  コストが同点の場合は、libmecab と同様に後の候補が選ばれます。
- BMP 外の文字分類は意図的に異なります(機能の節を参照)。
- テキストが空白で終わる場合、libmecab はテキストの末尾の先に行き止まりの未知語ノードを追加しますが、
  このクレートは追加しません。最適パスと N-best パスは同じです。`parse_lattice` ではノードが 1 つ少なく表示され、
  その手前のノードの周辺確率は約 1e-6 異なります。
- N-best パスは、どの順位でもコストが同じですが、コストが等しいパス同士の順序は異なることがあります。
- libmecab の C バッファによる制限(位置ごとのヒット数、ノード数、テキスト長)は一切ありません。
- 未実装: 出力フォーマット、および soft-wakati などの MeCab のその他のオプション。

## ベンチマーク

`compare/` は、以下の数値の測定に使ったベンチマークハーネスです。1 スレッド、mecab_rs・vibrato・lindera には
UniDic-CWJ 2025.12(sudachi.rs はこれを読み込めないため SudachiDict core を使用)、青空文庫の作品 91,000 文、
Ryzen 9 3900X、Windows 10、ウォームアップ 3 回と計測 9 回、中央値です。比較対象は
[vibrato](https://github.com/daac-tools/vibrato) 0.5.2、
[lindera](https://github.com/lindera/lindera)、
[sudachi.rs](https://github.com/WorksApplications/sudachi.rs) です。

| | ディスク上の辞書 | ロード | ピーク WS | ピークプライベート | 文/秒(表層形) | 文/秒(素性あり) |
|---|---:|---:|---:|---:|---:|---:|
| mecab_rs | 84.4 MB (`.mrd`), 6.39 MB (`.mrd.xz`) | 24 ms | 92.0 MB | 14.4 MB | 67,293 | 55,090 |
| vibrato | 407.2 MB | 13,558 ms | 1299 MB | 1305 MB | 34,999 | 33,463 |
| lindera | 1,305.3 MB | 3 ms | 569 MB | 19 MB | 28,189 | 19,903 |
| sudachi.rs | 202.2 MB | 33 ms | 163 MB | 17 MB | 25,187 | 27,634 |

`.mrd` のマップされたページはワーキングセット(WS)に含まれますが、プライベートメモリではなく共有のファイル
キャッシュです。mecab-ipadic 2.7.0 では、mecab_rs は表層形のみで vibrato の約 1.17 倍高速ですが、素性を
読み出す場合は vibrato のほうが約 1.04 倍高速です(mecab_rs は 224,443 / 173,867 文/秒、vibrato は
191,165 / 180,501 文/秒)。
mecab_rs の分かち書きは、すべての文で vibrato と一致し、lindera とは 99.8% の文で一致します。

## ライセンス

BSD-3-Clause。このクレートは MeCab を一行ずつ移植したもので、MeCab は BSD ライセンスで提供されています。
MeCab の著作権表示は `LICENSE` の先頭に残してあります。
