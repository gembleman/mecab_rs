use std::thread;

use super::lanes::lane_count;
use super::scan::split_fields;

const ESTIMATED_FIELDS: usize = 32;

#[derive(Default)]
struct SpanChunk {
    counts: Vec<u16>,
    ends: Vec<u32>,
}

fn scan_chunk(entries: &[&[u8]]) -> SpanChunk {
    let mut counts = Vec::with_capacity(entries.len());
    let mut ends = Vec::with_capacity(entries.len() * ESTIMATED_FIELDS);
    let mut fields = Vec::new();
    for text in entries {
        split_fields(text, &mut fields);
        counts.push(fields.len() as u16);
        ends.extend(fields.iter().map(|&(_, end)| end as u32));
    }
    SpanChunk { counts, ends }
}

fn scan_chunks(entries: &[&[u8]]) -> Vec<SpanChunk> {
    let lanes = lane_count(entries.len());
    if lanes == 1 {
        return vec![scan_chunk(entries)];
    }
    let chunk_len = entries.len().div_ceil(lanes);
    thread::scope(|scope| {
        let handles: Vec<_> = entries
            .chunks(chunk_len)
            .map(|part| scope.spawn(move || scan_chunk(part)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a field scanner panicked"))
            .collect()
    })
}

pub(super) struct Spans {
    ends: Vec<u32>,
    offsets: Vec<usize>,
}

impl Spans {
    pub(super) fn build(entries: &[&[u8]]) -> (Self, Vec<u16>) {
        let chunks = scan_chunks(entries);
        let total_ends: usize = chunks.iter().map(|chunk| chunk.ends.len()).sum();
        let mut chunks = chunks.into_iter();
        let first = chunks.next().unwrap_or_default();
        let mut ends = first.ends;
        ends.reserve_exact(total_ends - ends.len());
        let mut field_counts = first.counts;
        field_counts.reserve_exact(entries.len() - field_counts.len());
        for chunk in chunks {
            ends.extend_from_slice(&chunk.ends);
            field_counts.extend_from_slice(&chunk.counts);
        }
        let mut offsets = Vec::with_capacity(entries.len() + 1);
        let mut running = 0usize;
        offsets.push(running);
        for &count in &field_counts {
            running += usize::from(count);
            offsets.push(running);
        }
        (Self { ends, offsets }, field_counts)
    }

    pub(super) fn span(&self, entry: usize, field: usize) -> (usize, usize) {
        let base = self.offsets[entry];
        let begin = if field == 0 {
            0
        } else {
            self.ends[base + field - 1] as usize + 1
        };
        (begin, self.ends[base + field] as usize)
    }

    pub(super) fn fill(&self, entry: usize, fields: &mut Vec<(usize, usize)>) {
        fields.clear();
        let mut begin = 0usize;
        for &end in &self.ends[self.offsets[entry]..self.offsets[entry + 1]] {
            fields.push((begin, end as usize));
            begin = end as usize + 1;
        }
    }
}
