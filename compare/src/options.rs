use std::path::PathBuf;

pub struct Opts {
    pub tool: String,
    pub dict: PathBuf,
    pub corpus: PathBuf,
    pub warmup: usize,
    pub passes: usize,
    pub limit: Option<usize>,
    pub dump: Option<PathBuf>,
    pub max_grouping: usize,
}

pub fn parse_args() -> Opts {
    let mut tool = None;
    let mut dict = None;
    let mut corpus = None;
    let (mut warmup, mut passes, mut limit, mut dump) = (2, 7, None, None);
    let mut max_grouping = 24;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().unwrap_or_else(|| panic!("{k} needs a value"));
        match k.as_str() {
            "--tool" => tool = Some(v()),
            "--dict" => dict = Some(PathBuf::from(v())),
            "--corpus" => corpus = Some(PathBuf::from(v())),
            "--warmup" => warmup = v().parse().unwrap(),
            "--passes" => passes = v().parse().unwrap(),
            "--limit" => limit = Some(v().parse().unwrap()),
            "--dump" => dump = Some(PathBuf::from(v())),
            "--max-grouping" => max_grouping = v().parse().unwrap(),
            _ => panic!("unknown option {k}"),
        }
    }
    Opts {
        tool: tool.expect("--tool"),
        dict: dict.expect("--dict"),
        corpus: corpus.expect("--corpus"),
        warmup,
        passes,
        limit,
        dump,
        max_grouping,
    }
}
