pub const USAGE: &str = "usage: mecab-rs-migrate <dictionary-directory> [--char-def <file>]

Converts <dictionary-directory>/char.bin into <dictionary-directory>/char.uni. Range lines
above the BMP of char.def (the one in the directory, or the given file) are applied on top of
the built-in defaults; the other files of the directory are not touched.";

pub const REBUILD_USAGE: &str = "
dictionary rebuilding:
  mecab-rs-migrate pack <dict-dir> <out.mrd>
  mecab-rs-migrate pack-transport <dict-dir> <out.mrd.xz>
  mecab-rs-migrate expand <in.mrd.xz> <out.mrd>
      Writes a packed runtime dictionary, or packs and expands its transport form.
  mecab-rs-migrate check-runtime-int <matrix.itp> <runtime> [<matrix.bin>] [--threads N]
      Compares every cell of the runtime form of matrix.itp (what pack puts into the .mrd file)
      outside of its hot block with matrix.itp and, if given, with the dense matrix.bin.
  mecab-rs-migrate pair-stats <mrd> <corpus>... -o <file>  (feature pair-stats)
      Parses every line of the corpora with max grouping size 10 and writes how often the lattice
      asked for each (right node left id, left node right id) pair of the hot matrix as records
      of row u16, column u16, count u32 (little endian), for rebuild --pairs.
  mecab-rs-migrate rebuild <unidic-full-dir> <out-dir> (--stats <dir> | --corpus <file>...)
                         [--keep <fields>] [--threads N]
                         [--hot L R | --pairs <file> [--hot-cells N]]
                         [--matrix-bin]
      One-shot build of the runtime files only (dicrc, char.bin, char.uni, license/, sys.dic,
      unk.dic, left-id.def, right-id.def, right.tsv, left.tsv, matrix.itp, matrix.hot): renumbers
      by the stats (given, or counted from the corpora), slims with --keep, and compiles the
      integer templates matrix.itp (grid 2^-20, every cell exact) instead of writing matrix.bin;
      --matrix-bin also writes the permuted dense
      matrix.bin. --hot L R makes the hot part a rectangle; --pairs with --hot-cells N makes it a
      staircase of at most N cells (default 14,000,000) whose row lengths follow the pair counts.";
