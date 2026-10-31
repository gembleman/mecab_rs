use std::ops::Range;
use std::thread;

use crate::Error;

const MAX_THREADS: usize = 8;

pub(super) fn thread_count(wanted: bool) -> usize {
    if wanted {
        thread::available_parallelism().map_or(1, |count| count.get().min(MAX_THREADS))
    } else {
        1
    }
}

pub(super) fn sync_parts(sync_length: usize, threads: usize) -> Vec<(usize, usize)> {
    let per_part = sync_length.div_ceil(threads);
    (0..threads)
        .map(|number| {
            (
                number * per_part,
                ((number + 1) * per_part).min(sync_length),
            )
        })
        .filter(|&(first, last)| first < last)
        .collect()
}

pub(super) struct BitSink<'a> {
    words: &'a mut [u64],
    first_word: usize,
    tail: u64,
}

impl BitSink<'_> {
    #[inline]
    pub(super) fn set(&mut self, position: usize, condition: bool) {
        let bit = u64::from(condition) << (position & 63);
        match self.words.get_mut((position >> 6) - self.first_word) {
            Some(word) => *word |= bit,
            None => self.tail |= bit,
        }
    }
}

fn split_sinks<'a>(words: &'a mut [u64], bounds: &[usize]) -> Vec<BitSink<'a>> {
    let parts = bounds.len() - 1;
    let mut sinks = Vec::with_capacity(parts);
    let mut rest = words;
    for (number, pair) in bounds.windows(2).enumerate() {
        let first_word = pair[0] >> 6;
        let owned = if number + 1 == parts {
            rest.len()
        } else {
            (pair[1] >> 6) - first_word
        };
        let (mine, others) = rest.split_at_mut(owned);
        rest = others;
        sinks.push(BitSink {
            words: mine,
            first_word,
            tail: 0,
        });
    }
    sinks
}

fn run_parts<W>(sinks: &mut [BitSink], bounds: &[usize], work: &W) -> Vec<Result<(), Error>>
where
    W: Fn(usize, Range<usize>, &mut BitSink) -> Result<(), Error> + Sync,
{
    if sinks.len() == 1 {
        return vec![work(0, bounds[0]..bounds[1], &mut sinks[0])];
    }
    thread::scope(|scope| {
        let handles: Vec<_> = sinks
            .iter_mut()
            .zip(bounds.windows(2))
            .enumerate()
            .map(|(number, (sink, pair))| {
                let range = pair[0]..pair[1];
                scope.spawn(move || work(number, range, sink))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a check thread panicked"))
            .collect()
    })
}

fn merge_tails(words: &mut [u64], bounds: &[usize], tails: &[u64]) {
    let parts = bounds.len() - 1;
    for (number, &tail) in tails.iter().enumerate().take(parts - 1) {
        words[bounds[number + 1] >> 6] |= tail;
    }
}

pub(super) fn fill_bits<W>(words: &mut [u64], bounds: &[usize], work: W) -> Result<(), Error>
where
    W: Fn(usize, Range<usize>, &mut BitSink) -> Result<(), Error> + Sync,
{
    let mut sinks = split_sinks(words, bounds);
    let results = run_parts(&mut sinks, bounds, &work);
    let tails: Vec<u64> = sinks.iter().map(|sink| sink.tail).collect();
    drop(sinks);
    for result in results {
        result?;
    }
    merge_tails(words, bounds, &tails);
    Ok(())
}

pub(super) fn part_bounds(parts: &[(usize, usize)], sync: &[usize], end: usize) -> Vec<usize> {
    let mut bounds: Vec<usize> = parts.iter().map(|&(first, _)| sync[first]).collect();
    bounds.push(end);
    bounds
}
