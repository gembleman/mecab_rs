use std::ffi::OsString;
use std::path::{Path, PathBuf};

use mecab_rs::__private::matrix::HotShape;
use mecab_rs_tools::cli_options::{Arity, NamedSpec, Options, lossy_arguments};
use mecab_rs_tools::rebuild::{self, PAIR_COUNT_BYTES, PairCount, hot_shape_from_pair_counts};

const HOT_SPECS: [NamedSpec; 8] = [
    NamedSpec {
        name: "stats",
        arity: Arity::Text,
    },
    NamedSpec {
        name: "corpus",
        arity: Arity::Files,
    },
    NamedSpec {
        name: "keep",
        arity: Arity::Text,
    },
    NamedSpec {
        name: "hot",
        arity: Arity::Numbers(2),
    },
    NamedSpec {
        name: "hot-cells",
        arity: Arity::Numbers(1),
    },
    NamedSpec {
        name: "threads",
        arity: Arity::Numbers(1),
    },
    NamedSpec {
        name: "pairs",
        arity: Arity::Text,
    },
    NamedSpec {
        name: "matrix-bin",
        arity: Arity::Flag,
    },
];

const HOT_CONFLICT: &str = "rebuild: give --pairs <file> with --hot-cells <N>, or --hot L R";

pub struct MatrixArgs {
    pub positional: Vec<PathBuf>,
    pub stats: Option<PathBuf>,
    pub corpora: Vec<PathBuf>,
    pub keep: Option<rebuild::Keep>,
    pub hot: Option<(usize, usize)>,
    pub hot_cells: Option<usize>,
    pub threads: Option<usize>,
    pub matrix_bin: bool,
    pub pairs: Option<PathBuf>,
}

fn paths(texts: &[String]) -> Vec<PathBuf> {
    texts.iter().map(PathBuf::from).collect()
}

impl MatrixArgs {
    pub fn parse(command: &str, args: &[OsString]) -> Result<Self, String> {
        let options = Options::parse_named(command, &lossy_arguments(args), &HOT_SPECS)?;
        let keep = options
            .get("keep")
            .map(rebuild::Keep::parse_fields)
            .transpose()?;
        let hot = match options.numbers("hot")[..] {
            [left, right] => Some((left, right)),
            _ => None,
        };
        Ok(Self {
            positional: paths(&options.rest),
            stats: options.get("stats").map(PathBuf::from),
            corpora: paths(options.all("corpus")),
            keep,
            hot,
            hot_cells: options.optional_number("hot-cells"),
            threads: options.optional_number("threads"),
            matrix_bin: options.flag("matrix-bin"),
            pairs: options.get("pairs").map(PathBuf::from),
        })
    }

    pub fn threads(&self) -> usize {
        self.threads.unwrap_or_else(rebuild::default_threads)
    }

    pub fn hot_shape(&self) -> Result<Option<HotShape>, String> {
        match (&self.pairs, self.hot_cells, self.hot) {
            (Some(pairs), cells, None) => {
                let cells = cells.unwrap_or(rebuild::DEFAULT_HOT_CELLS);
                let shape = hot_shape_from_pair_counts(&read_pair_counts(pairs)?, cells);
                println!(
                    "hot shape from pair counts: {} cells in {} rows, inscribed rectangle {:?}",
                    shape.cells(),
                    shape.rows(),
                    shape.inscribed_rectangle()
                );
                Ok(Some(shape))
            }
            (None, None, hot) => Ok(hot.map(|(left, right)| HotShape::rectangle(left, right))),
            _ => Err(HOT_CONFLICT.into()),
        }
    }
}

pub fn positional<'a, const COUNT: usize>(
    command: &str,
    positional: &'a [PathBuf],
    usage: &str,
) -> Result<[&'a PathBuf; COUNT], String> {
    let found: Vec<&PathBuf> = positional.iter().collect();
    found
        .try_into()
        .map_err(|_| format!("{command}: expected {usage}"))
}

fn read_pair_counts(path: &Path) -> Result<Vec<PairCount>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let (chunks, rest) = bytes.as_chunks::<{ PAIR_COUNT_BYTES }>();
    if !rest.is_empty() {
        return Err(format!("{}: not a list of pair counts", path.display()));
    }
    Ok(chunks
        .iter()
        .map(|&chunk| PairCount::from_bytes(chunk))
        .collect())
}
