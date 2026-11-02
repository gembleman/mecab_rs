use std::thread;

use super::MAX_FIELDS;
use super::lanes::lane_count;
use super::scan::split_fields;

const OVERFLOW_LENGTH: u8 = u8::MAX;

trait EndWord: Copy + Default + Send {
    fn pack(begin: usize, end: usize) -> Self;
    fn unpack(self) -> usize;
}

impl EndWord for u8 {
    fn pack(begin: usize, end: usize) -> Self {
        debug_assert!(begin <= end);
        u8::try_from(end - begin).unwrap_or(OVERFLOW_LENGTH)
    }

    fn unpack(self) -> usize {
        usize::from(self)
    }
}

impl EndWord for u16 {
    fn pack(_begin: usize, end: usize) -> Self {
        debug_assert!(u16::try_from(end).is_ok());
        end as u16
    }

    fn unpack(self) -> usize {
        usize::from(self)
    }
}

impl EndWord for u32 {
    fn pack(_begin: usize, end: usize) -> Self {
        debug_assert!(u32::try_from(end).is_ok());
        end as u32
    }

    fn unpack(self) -> usize {
        self as usize
    }
}

enum SpanEnds {
    Lengths(Vec<u8>),
    Narrow(Vec<u16>),
    Wide(Vec<u32>),
}

impl SpanEnds {
    fn at(&self, index: usize) -> usize {
        match self {
            Self::Lengths(_) => unreachable!("lengths are summed, not indexed"),
            Self::Narrow(ends) => ends[index].unpack(),
            Self::Wide(ends) => ends[index].unpack(),
        }
    }
}

#[allow(
    clippy::naive_bytecount,
    reason = "a plain count over one short field; no bytecount dependency"
)]
fn field_bound(text: &[u8]) -> usize {
    let commas = text.iter().filter(|&&byte| byte == b',').count();
    (commas + 1).min(MAX_FIELDS)
}

fn part_bound(entries: &[&[u8]]) -> usize {
    entries.iter().map(|text| field_bound(text)).sum()
}

fn part_bounds(parts: &[&[&[u8]]]) -> Vec<usize> {
    if parts.len() <= 1 {
        return parts.iter().map(|part| part_bound(part)).collect();
    }
    thread::scope(|scope| {
        let handles: Vec<_> = parts
            .iter()
            .map(|&part| scope.spawn(move || part_bound(part)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a field counter panicked"))
            .collect()
    })
}

fn scan_part<T: EndWord>(entries: &[&[u8]], counts: &mut [u16], ends: &mut [T]) -> usize {
    let mut fields = Vec::new();
    let mut written = 0usize;
    for (text, count) in entries.iter().zip(counts) {
        split_fields(text, &mut fields);
        *count = fields.len() as u16;
        let room = &mut ends[written..written + fields.len()];
        for (slot, &(begin, end)) in room.iter_mut().zip(&fields) {
            *slot = T::pack(begin, end);
        }
        written += fields.len();
    }
    written
}

struct PartJob<'e, 'c, 'o, T> {
    entries: &'e [&'e [u8]],
    counts: &'c mut [u16],
    ends: &'o mut [T],
}

fn scan_jobs<T: EndWord>(jobs: Vec<PartJob<'_, '_, '_, T>>) -> Vec<usize> {
    if jobs.len() <= 1 {
        return jobs
            .into_iter()
            .map(|job| scan_part(job.entries, job.counts, job.ends))
            .collect();
    }
    thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| scope.spawn(move || scan_part(job.entries, job.counts, job.ends)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a field scanner panicked"))
            .collect()
    })
}

fn compact<T: Copy>(ends: &mut Vec<T>, bounds: &[usize], written: &[usize]) {
    let mut to = 0usize;
    let mut from = 0usize;
    for (&bound, &length) in bounds.iter().zip(written) {
        ends.copy_within(from..from + length, to);
        to += length;
        from += bound;
    }
    ends.truncate(to);
    ends.shrink_to_fit();
}

fn scan_all<T: EndWord>(entries: &[&[u8]], counts: &mut [u16]) -> Vec<T> {
    let chunk_len = entries.len().div_ceil(lane_count(entries.len())).max(1);
    let parts: Vec<&[&[u8]]> = entries.chunks(chunk_len).collect();
    let bounds = part_bounds(&parts);
    let mut ends = vec![T::default(); bounds.iter().sum()];
    let mut rest: &mut [T] = &mut ends;
    let mut jobs = Vec::with_capacity(parts.len());
    for ((&part, count_part), &bound) in parts.iter().zip(counts.chunks_mut(chunk_len)).zip(&bounds)
    {
        let (mine, tail) = std::mem::take(&mut rest).split_at_mut(bound);
        rest = tail;
        jobs.push(PartJob {
            entries: part,
            counts: count_part,
            ends: mine,
        });
    }
    let written = scan_jobs(jobs);
    if written != bounds {
        compact(&mut ends, &bounds, &written);
    }
    ends
}

fn longest(entries: &[&[u8]]) -> usize {
    entries.iter().map(|text| text.len()).max().unwrap_or(0)
}

fn push_fields<T: EndWord>(ends: &[T], fields: &mut Vec<(usize, usize)>) {
    let mut begin = 0usize;
    for &word in ends {
        let end = word.unpack();
        fields.push((begin, end));
        begin = end + 1;
    }
}

fn lengths_extent(lengths: &[u8]) -> usize {
    let total: usize = lengths.iter().map(|&length| usize::from(length)).sum();
    total + lengths.len()
}

fn push_lengths(lengths: &[u8], fields: &mut Vec<(usize, usize)>) {
    let mut begin = 0usize;
    for &length in lengths {
        let end = begin + usize::from(length);
        fields.push((begin, end));
        begin = end + 1;
    }
}

fn scan_narrowest(entries: &[&[u8]], field_counts: &mut [u16]) -> SpanEnds {
    if longest(entries) < usize::from(OVERFLOW_LENGTH) {
        return SpanEnds::Lengths(scan_all(entries, field_counts));
    }
    let lengths = scan_all::<u8>(entries, field_counts);
    if !lengths.contains(&OVERFLOW_LENGTH) {
        return SpanEnds::Lengths(lengths);
    }
    drop(lengths);
    if u16::try_from(longest(entries)).is_ok() {
        SpanEnds::Narrow(scan_all(entries, field_counts))
    } else {
        SpanEnds::Wide(scan_all(entries, field_counts))
    }
}

pub(super) struct Spans {
    ends: SpanEnds,
    offsets: Vec<u32>,
}

impl Spans {
    pub(super) fn build(entries: &[&[u8]]) -> (Self, Vec<u16>) {
        let mut field_counts = vec![0u16; entries.len()];
        let ends = scan_narrowest(entries, &mut field_counts);
        let mut offsets = Vec::with_capacity(entries.len() + 1);
        let mut running = 0usize;
        offsets.push(0);
        for &count in &field_counts {
            running += usize::from(count);
            debug_assert!(u32::try_from(running).is_ok());
            offsets.push(running as u32);
        }
        (Self { ends, offsets }, field_counts)
    }

    pub(super) fn span(&self, entry: usize, field: usize) -> (usize, usize) {
        let base = self.offsets[entry] as usize;
        if let SpanEnds::Lengths(lengths) = &self.ends {
            let begin = lengths_extent(&lengths[base..base + field]);
            return (begin, begin + usize::from(lengths[base + field]));
        }
        let begin = if field == 0 {
            0
        } else {
            self.ends.at(base + field - 1) + 1
        };
        (begin, self.ends.at(base + field))
    }

    pub(super) fn field_count(&self, entry: usize) -> usize {
        (self.offsets[entry + 1] - self.offsets[entry]) as usize
    }

    pub(super) fn fill(&self, entry: usize, fields: &mut Vec<(usize, usize)>) {
        fields.clear();
        let range = self.offsets[entry] as usize..self.offsets[entry + 1] as usize;
        match &self.ends {
            SpanEnds::Lengths(lengths) => push_lengths(&lengths[range], fields),
            SpanEnds::Narrow(ends) => push_fields(&ends[range], fields),
            SpanEnds::Wide(ends) => push_fields(&ends[range], fields),
        }
    }
}
