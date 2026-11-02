use std::hash::{BuildHasherDefault, Hasher};
use std::{mem, thread};

use super::lanes::lane_count;
use super::plans::{is_new_code, plan_code};
use super::{Error, NO_STRING, NO_VARIANT, Planner, Spans, TextOp, Tokens};
use crate::hash::{FastHasher, FastSet};

const WINDOW_TOKENS: usize = 1 << 17;
const BATCH_VARIANTS: usize = 1 << 15;
const SHARD_HASH_SHIFT: u32 = 24;
const SHARD_HASH_BITS: u32 = 16;
const FLAG_KEYED: u8 = 1;
const FLAG_LOCALLY_NEW: u8 = 2;
const EMPTY_SLOT: u32 = u32::MAX;
const MIN_SLOTS: usize = 16;
const MAX_HASH_SLOTS: usize = 1 << 32;
const SLOTS_PER_ITEM: usize = 2;

pub(super) struct Variants {
    pub(super) list: Vec<u32>,
    pub(super) starts_of: Vec<u32>,
    pub(super) uses_of_id: Vec<u32>,
    pub(super) segments: Vec<Segment>,
}

pub(super) struct Segment {
    pub(super) variants: usize,
    pub(super) plan_codes: Vec<u8>,
    pub(super) string_ids: Vec<u32>,
}

impl Segment {
    fn with_room_for(pending: &Pending<'_>, spans: &Spans) -> Self {
        let mut fields = 0usize;
        let mut new_strings = 0usize;
        for &(entry, plan) in pending {
            let used = &plan[..spans.field_count(entry).min(plan.len())];
            fields += used.len();
            new_strings += used.iter().filter(|&&code| is_new_code(code)).count();
        }
        Self {
            variants: pending.len(),
            plan_codes: Vec::with_capacity(fields),
            string_ids: Vec::with_capacity(new_strings),
        }
    }
}

type Pending<'p> = Vec<(usize, &'p [u8])>;

struct SlotTable {
    slots: Vec<u32>,
    used: usize,
}

impl Default for SlotTable {
    fn default() -> Self {
        Self::with_items(0)
    }
}

impl SlotTable {
    fn with_items(items: usize) -> Self {
        let slots = items
            .saturating_mul(SLOTS_PER_ITEM)
            .next_power_of_two()
            .max(MIN_SLOTS);
        Self {
            slots: vec![EMPTY_SLOT; slots],
            used: 0,
        }
    }

    fn find_or_insert(
        &mut self,
        hash: u64,
        next: u32,
        equals: impl Fn(u32) -> bool,
        hash_of: impl Fn(u32) -> u64,
    ) -> u32 {
        if (self.used + 1) * SLOTS_PER_ITEM > self.slots.len() {
            self.grow(hash_of);
        }
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot == EMPTY_SLOT {
                self.slots[at] = next;
                self.used += 1;
                return next;
            }
            if equals(slot) {
                return slot;
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self, hash_of: impl Fn(u32) -> u64) {
        let mut slots = vec![EMPTY_SLOT; self.slots.len() * 2];
        let mask = slots.len() - 1;
        for &number in self.slots.iter().filter(|&&slot| slot != EMPTY_SLOT) {
            let mut at = hash_of(number) as usize & mask;
            while slots[at] != EMPTY_SLOT {
                at = (at + 1) & mask;
            }
            slots[at] = number;
        }
        self.slots = slots;
    }
}

#[derive(Default)]
struct ChunkPlans {
    flags: Vec<u8>,
    plan_lens: Vec<u16>,
    ops: Vec<u8>,
}

fn plan_chunk<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    tokens: Tokens<'_>,
    planner: &Planner<'_, 'a>,
) -> Result<ChunkPlans, Error> {
    let mut plans = ChunkPlans {
        flags: Vec::with_capacity(tokens.len()),
        ..ChunkPlans::default()
    };
    let mut seen: FastSet<(u32, &[u8])> =
        FastSet::with_capacity_and_hasher(tokens.len(), BuildHasherDefault::default());
    let mut planned: Vec<Option<TextOp>> = Vec::new();
    let mut slots: Vec<&[u8]> = Vec::new();
    let mut fields = Vec::new();
    let mut filled_entry = usize::MAX;
    for (feature, key) in tokens.iter() {
        let entry = feature as usize;
        let text = *entries.get(entry).ok_or(Error::Unsupported(
            "a token with a feature that is not there",
        ))?;
        if filled_entry != entry {
            spans.fill(entry, &mut fields);
            filled_entry = entry;
        }
        planner.plan(
            entry,
            text,
            &fields,
            key.as_bytes(),
            &mut slots,
            &mut planned,
        );
        let keyed = planner.uses_key(&planned);
        let variant_key: &[u8] = if keyed { key.as_bytes() } else { b"" };
        let locally_new = seen.insert((feature, variant_key));
        let mut flags = 0u8;
        if keyed {
            flags |= FLAG_KEYED;
        }
        if locally_new {
            flags |= FLAG_LOCALLY_NEW;
            plans.plan_lens.push(planned.len() as u16);
            plans.ops.extend(planned.iter().map(|&op| plan_code(op)));
        }
        plans.flags.push(flags);
    }
    Ok(plans)
}

fn plan_window<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    window: Tokens<'_>,
    planner: &Planner<'_, 'a>,
    lanes: usize,
) -> Result<Vec<ChunkPlans>, Error> {
    let chunk_len = window.len().div_ceil(lanes.max(1)).max(1);
    if lanes <= 1 || window.len() <= chunk_len {
        return Ok(vec![plan_chunk(entries, spans, window, planner)?]);
    }
    let results: Vec<Result<ChunkPlans, Error>> = thread::scope(|scope| {
        let handles: Vec<_> = window
            .chunks(chunk_len)
            .map(|chunk| scope.spawn(move || plan_chunk(entries, spans, chunk, planner)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    results.into_iter().collect()
}

fn variant_hash(feature: u32, key: &[u8]) -> u64 {
    let mut hasher = FastHasher::default();
    hasher.write(key);
    hasher.write(&feature.to_le_bytes());
    hasher.finish()
}

struct Numbering<'k> {
    tokens: Tokens<'k>,
    token_of: Vec<u32>,
    hash_of: Vec<u32>,
    table: SlotTable,
    plain_variant: Vec<u32>,
    starts_of: Vec<u32>,
}

impl<'k> Numbering<'k> {
    fn new(tokens: Tokens<'k>, entry_count: usize) -> Self {
        Self {
            tokens,
            token_of: Vec::new(),
            hash_of: Vec::new(),
            table: SlotTable::with_items(tokens.len()),
            plain_variant: vec![NO_VARIANT; entry_count],
            starts_of: Vec::with_capacity(tokens.len()),
        }
    }

    fn into_lists(self) -> (Vec<u32>, Vec<u32>) {
        let tokens = self.tokens;
        let list: Vec<u32> = self
            .token_of
            .iter()
            .map(|&token| tokens.at(token as usize).0)
            .collect();
        (list, self.starts_of)
    }

    fn keyed_number(&mut self, token: usize, hash: u64, next: u32) -> u32 {
        let tokens = self.tokens;
        let (feature, key) = tokens.at(token);
        let key = key.as_bytes();
        let token_of = &self.token_of;
        let hash_of = &self.hash_of;
        let short_hash = hash as u32;
        debug_assert!(self.table.slots.len() <= MAX_HASH_SLOTS);
        self.table.find_or_insert(
            hash,
            next,
            |number| {
                let (other_feature, other_key) = tokens.at(token_of[number as usize] as usize);
                hash_of[number as usize] == short_hash
                    && other_feature == feature
                    && other_key.as_bytes() == key
            },
            |number| u64::from(hash_of[number as usize]),
        )
    }

    fn number_chunk<'p>(
        &mut self,
        first: usize,
        plans: &'p ChunkPlans,
        pending: &mut Pending<'p>,
    ) -> Result<(), Error> {
        let mut plan_index = 0usize;
        let mut ops_at = 0usize;
        for (offset, &flags) in plans.flags.iter().enumerate() {
            let token = first + offset;
            let (feature, key) = self.tokens.at(token);
            let entry = feature as usize;
            let key: &[u8] = if flags & FLAG_KEYED != 0 {
                key.as_bytes()
            } else {
                b""
            };
            let planned = if flags & FLAG_LOCALLY_NEW != 0 {
                let len = usize::from(plans.plan_lens[plan_index]);
                plan_index += 1;
                let planned = &plans.ops[ops_at..ops_at + len];
                ops_at += len;
                Some(planned)
            } else {
                None
            };
            let next = self.token_of.len() as u32;
            let mut hash = 0u64;
            let number = if key.is_empty() {
                let slot = &mut self.plain_variant[entry];
                if *slot == NO_VARIANT {
                    *slot = next;
                }
                *slot
            } else {
                hash = variant_hash(feature, key);
                self.keyed_number(token, hash, next)
            };
            if number == next {
                let planned =
                    planned.ok_or(Error::Unsupported("a new feature variant without a plan"))?;
                self.token_of.push(token as u32);
                self.hash_of.push(hash as u32);
                pending.push((entry, planned));
            }
            self.starts_of.push(number);
        }
        Ok(())
    }
}

fn hash_text(text: &[u8]) -> u64 {
    let mut hasher = FastHasher::default();
    hasher.write(text);
    hasher.finish()
}

fn shard_of(hash: u64, shards: usize) -> usize {
    let spread = (hash >> SHARD_HASH_SHIFT) & ((1 << SHARD_HASH_BITS) - 1);
    ((spread * shards as u64) >> SHARD_HASH_BITS) as usize
}

struct Occurrence<'a> {
    hash: u64,
    text: &'a [u8],
    at: u32,
    key: u32,
}

#[derive(Default)]
struct Shard<'a> {
    table: SlotTable,
    hash_of_key: Vec<u64>,
    text_of_key: Vec<&'a [u8]>,
    uses_of_key: Vec<u32>,
    id_of_key: Vec<u32>,
    batch: Vec<Vec<Occurrence<'a>>>,
    fresh: Vec<(u32, u32, u32)>,
}

impl<'a> Shard<'a> {
    fn key_of(&mut self, hash: u64, text: &'a [u8]) -> (u32, bool) {
        let next = self.uses_of_key.len() as u32;
        let hashes = &self.hash_of_key;
        let texts = &self.text_of_key;
        let key = self.table.find_or_insert(
            hash,
            next,
            |key| hashes[key as usize] == hash && texts[key as usize] == text,
            |key| hashes[key as usize],
        );
        let is_new = key == next;
        if is_new {
            self.hash_of_key.push(hash);
            self.text_of_key.push(text);
            self.uses_of_key.push(0);
            self.id_of_key.push(NO_STRING);
        }
        (key, is_new)
    }

    fn number_batch(&mut self) {
        let mut batch = mem::take(&mut self.batch);
        for (chunk_index, occurrences) in batch.iter_mut().enumerate() {
            for occurrence in occurrences {
                let (key, is_new) = self.key_of(occurrence.hash, occurrence.text);
                if is_new {
                    self.fresh.push((chunk_index as u32, occurrence.at, key));
                }
                self.uses_of_key[key as usize] += 1;
                occurrence.key = key;
            }
        }
        self.batch = batch;
    }
}

struct Chunk<'b, 'a, 'o> {
    variants: &'b [(usize, &'b [u8])],
    codes: &'o mut [u8],
    ids: Vec<u32>,
    buckets: Vec<Vec<Occurrence<'a>>>,
}

fn run_parallel<T: Send>(items: &mut [T], visit: impl Fn(usize, &mut T) + Sync) {
    if items.len() <= 1 {
        for (index, item) in items.iter_mut().enumerate() {
            visit(index, item);
        }
        return;
    }
    let visit = &visit;
    thread::scope(|scope| {
        for (index, item) in items.iter_mut().enumerate() {
            scope.spawn(move || visit(index, item));
        }
    });
}

fn split_by_lengths<'s, T>(mut rest: &'s mut [T], lengths: &[usize]) -> Vec<&'s mut [T]> {
    let mut parts = Vec::with_capacity(lengths.len());
    for &length in lengths {
        let (part, tail) = rest.split_at_mut(length);
        parts.push(part);
        rest = tail;
    }
    parts
}

fn fill_chunk<'a>(
    chunk: &mut Chunk<'_, 'a, '_>,
    entries: &[&'a [u8]],
    spans: &Spans,
    shards: usize,
) {
    chunk.buckets = (0..shards).map(|_| Vec::new()).collect();
    let mut fields = Vec::new();
    let mut at = 0usize;
    let mut ordinal = 0u32;
    for &(entry, plan) in chunk.variants {
        spans.fill(entry, &mut fields);
        let text: &'a [u8] = entries[entry];
        for (&(begin, end), &code) in fields.iter().zip(plan) {
            chunk.codes[at] = code;
            if is_new_code(code) {
                let field = &text[begin..end];
                let hash = hash_text(field);
                chunk.buckets[shard_of(hash, shards)].push(Occurrence {
                    hash,
                    text: field,
                    at: ordinal,
                    key: 0,
                });
                ordinal += 1;
            }
            at += 1;
        }
    }
    chunk.ids = vec![NO_STRING; ordinal as usize];
}

fn write_ids(chunk: &mut Chunk<'_, '_, '_>, index: usize, shards: &[Shard<'_>]) {
    for shard in shards {
        for occurrence in &shard.batch[index] {
            chunk.ids[occurrence.at as usize] = shard.id_of_key[occurrence.key as usize];
        }
    }
}

fn assign_ids(shards: &mut [Shard<'_>], next_id: &mut u32) {
    let mut fresh: Vec<(u32, u32, u32, u32)> = Vec::new();
    for (shard_index, shard) in shards.iter_mut().enumerate() {
        fresh.extend(
            shard
                .fresh
                .drain(..)
                .map(|(chunk, at, key)| (chunk, at, shard_index as u32, key)),
        );
    }
    fresh.sort_unstable_by_key(|&(chunk, at, _, _)| (chunk, at));
    for (_, _, shard_index, key) in fresh {
        shards[shard_index as usize].id_of_key[key as usize] = *next_id;
        *next_id += 1;
    }
}

struct Emitter<'a> {
    lanes: usize,
    batch_variants: usize,
    shards: Vec<Shard<'a>>,
    next_id: u32,
    segments: Vec<Segment>,
}

impl<'a> Emitter<'a> {
    fn new(lanes: usize, batch_variants: usize) -> Self {
        let lanes = lanes.max(1);
        Self {
            lanes,
            batch_variants: batch_variants.max(1),
            shards: (0..lanes).map(|_| Shard::default()).collect(),
            next_id: 0,
            segments: Vec::new(),
        }
    }

    fn emit(&mut self, pending: &Pending<'_>, entries: &[&'a [u8]], spans: &Spans) {
        if pending.is_empty() {
            return;
        }
        let mut segment = Segment::with_room_for(pending, spans);
        for batch in pending.chunks(self.batch_variants) {
            self.emit_batch(batch, entries, spans, &mut segment);
        }
        self.segments.push(segment);
    }

    fn emit_batch(
        &mut self,
        batch: &[(usize, &[u8])],
        entries: &[&'a [u8]],
        spans: &Spans,
        segment: &mut Segment,
    ) {
        let lengths: Vec<usize> = batch
            .iter()
            .map(|&(entry, plan)| spans.field_count(entry).min(plan.len()))
            .collect();
        let chunk_len = batch.len().div_ceil(self.lanes).max(1);
        let chunk_fields: Vec<usize> = lengths
            .chunks(chunk_len)
            .map(|part| part.iter().sum())
            .collect();
        let total: usize = chunk_fields.iter().sum();
        let start = segment.plan_codes.len();
        segment.plan_codes.resize(start + total, 0);
        let codes = split_by_lengths(&mut segment.plan_codes[start..], &chunk_fields);
        let mut chunks: Vec<Chunk> = batch
            .chunks(chunk_len)
            .zip(codes)
            .map(|(variants, codes)| Chunk {
                variants,
                codes,
                ids: Vec::new(),
                buckets: Vec::new(),
            })
            .collect();
        let shard_count = self.shards.len();
        run_parallel(&mut chunks, |_, chunk| {
            fill_chunk(chunk, entries, spans, shard_count);
        });
        for (shard_index, shard) in self.shards.iter_mut().enumerate() {
            shard.batch = chunks
                .iter_mut()
                .map(|chunk| mem::take(&mut chunk.buckets[shard_index]))
                .collect();
        }
        run_parallel(&mut self.shards, |_, shard| shard.number_batch());
        assign_ids(&mut self.shards, &mut self.next_id);
        let shards = &self.shards;
        run_parallel(&mut chunks, |index, chunk| write_ids(chunk, index, shards));
        for shard in &mut self.shards {
            shard.batch.clear();
        }
        for chunk in &chunks {
            segment.string_ids.extend_from_slice(&chunk.ids);
        }
    }

    fn uses_of_id(&self) -> Vec<u32> {
        let mut uses_of_id = vec![0u32; self.next_id as usize];
        for shard in &self.shards {
            for (&id, &uses) in shard.id_of_key.iter().zip(&shard.uses_of_key) {
                uses_of_id[id as usize] = uses;
            }
        }
        uses_of_id
    }
}

pub(super) fn collect_variants<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    tokens: Tokens<'_>,
    planner: &Planner<'_, 'a>,
) -> Result<Variants, Error> {
    collect_windowed(
        entries,
        spans,
        tokens,
        planner,
        WINDOW_TOKENS,
        lane_count(tokens.len()),
        BATCH_VARIANTS,
    )
}

fn collect_windowed<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    tokens: Tokens<'_>,
    planner: &Planner<'_, 'a>,
    window_tokens: usize,
    lanes: usize,
    batch_variants: usize,
) -> Result<Variants, Error> {
    let mut numbering = Numbering::new(tokens, entries.len());
    let mut emitter = Emitter::new(lanes, batch_variants);
    let window_tokens = window_tokens.max(1);
    for (window_index, window) in tokens.chunks(window_tokens).enumerate() {
        let window_first = window_index.saturating_mul(window_tokens);
        let chunk_len = window.len().div_ceil(lanes.max(1)).max(1);
        let chunks = plan_window(entries, spans, window, planner, lanes)?;
        let mut pending = Pending::new();
        for (chunk_index, plans) in chunks.iter().enumerate() {
            let first = window_first + chunk_index * chunk_len;
            numbering.number_chunk(first, plans, &mut pending)?;
        }
        emitter.emit(&pending, entries, spans);
    }
    let (list, starts_of) = numbering.into_lists();
    let uses_of_id = emitter.uses_of_id();
    Ok(Variants {
        list,
        starts_of,
        uses_of_id,
        segments: emitter.segments,
    })
}
