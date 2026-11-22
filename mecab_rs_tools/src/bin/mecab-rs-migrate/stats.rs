use std::ffi::OsString;
use std::path::PathBuf;

use mecab_rs_tools::rebuild::{PAIR_COUNT_BYTES, PairCount};

use crate::shown;

pub fn pair_stats(args: &[OsString]) -> Result<(), String> {
    const STATS_MAX_GROUPING: std::num::NonZeroUsize = std::num::NonZeroUsize::new(10).unwrap();
    let mut output = None;
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut iterator = args.iter();
    while let Some(argument) = iterator.next() {
        if argument == "-o" {
            output = Some(PathBuf::from(
                iterator.next().ok_or("pair-stats: -o needs a file")?,
            ));
        } else {
            inputs.push(argument.into());
        }
    }
    let output = output.ok_or("pair-stats: give the output file with -o")?;
    let Some((runtime, corpora)) = inputs.split_first().filter(|(_, rest)| !rest.is_empty()) else {
        return Err("pair-stats: expected <mrd> <corpus>... -o <file>".into());
    };
    let mut dictionary = mecab_rs::Dictionary::open(runtime).map_err(shown)?;
    dictionary.set_max_grouping_size(STATS_MAX_GROUPING);
    let mut tagger = mecab_rs::Tagger::new(&dictionary);
    let mut sentences = 0usize;
    for corpus in corpora {
        let bytes =
            std::fs::read(corpus).map_err(|error| format!("{}: {error}", corpus.display()))?;
        let text =
            std::str::from_utf8(&bytes).map_err(|_| format!("{}: not UTF-8", corpus.display()))?;
        for line in text.split_terminator('\n') {
            let sentence = line.strip_suffix('\r').unwrap_or(line);
            tagger.parse(sentence).map_err(shown)?;
            sentences += 1;
        }
    }
    let pairs: Vec<PairCount> = mecab_rs::__private::support::take_pair_counts()
        .into_iter()
        .map(|(row, column, count)| PairCount { row, column, count })
        .collect();
    let mut bytes = Vec::with_capacity(pairs.len() * PAIR_COUNT_BYTES);
    for pair in &pairs {
        bytes.extend_from_slice(&pair.to_bytes());
    }
    std::fs::write(&output, &bytes).map_err(|error| format!("{}: {error}", output.display()))?;
    println!(
        "{sentences} sentences, {} distinct pairs, {} lookups -> {}",
        pairs.len(),
        pairs.iter().map(|pair| u64::from(pair.count)).sum::<u64>(),
        output.display()
    );
    Ok(())
}
