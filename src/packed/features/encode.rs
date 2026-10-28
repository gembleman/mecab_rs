use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use super::{
    BLOB_SYNC_EVERY, BitWriter, Counts, Layout, MAX_BLOB_FIELDS, MAX_ID_BYTES, MAX_NUMERIC,
    MAX_RULE_SUFFIX, MAX_RULES, MAX_SLOTS, NumSpec, OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_DERIVE,
    OP_EMPTY, OP_HIGH, OP_INLINE, OP_KEY, OP_NUM, OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF,
    OP_STAR, RULE_BYTES, RULE_CHAR_BITS, RULE_HEADER_BYTES, SYNC_EVERY, bits_for, chars,
    pieces::Pieces, push_piece,
};
use crate::Error;

mod blob;
mod derive;
mod numbers;
mod program;
mod records;
mod scan;
mod stats;

use blob::*;
use derive::*;
use numbers::*;
use program::*;
use records::*;
pub(in crate::packed) use scan::split_fields;
use scan::*;
use stats::*;

const MAX_FIELDS: usize = 256;
const FOLD_MAX_VALUES: usize = 256;
const MAX_TEMPLATES: usize = 1 << 16;
const MAX_LOW_PARTS: usize = 1 << 16;
const ABSENT: u16 = u16::MAX;
const DERIVE_TARGET: usize = 10;
const DERIVE_CONTEXT: [usize; 2] = [4, 5];
const MIN_RULE_USES: u32 = 2;
const MAX_REMOVED_CHARS: usize = 15;
const HASH_MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;
const FINAL_MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

pub(in crate::packed) struct Encoded {
    pub(in crate::packed) bytes: Vec<u8>,
    pub(in crate::packed) starts: Vec<u32>,
}

fn blob_slot(blob: Option<&Blob>, entry: usize, field: usize) -> Option<usize> {
    blob.and_then(|blob| blob.slot(entry, field))
}

fn split_entries(features: &[u8]) -> Result<Vec<&[u8]>, Error> {
    let mut entries: Vec<&[u8]> = features.split(|&byte| byte == 0).collect();
    entries.pop();
    if u32::try_from(entries.len()).is_err() {
        return Err(Error::Unsupported("more than 2^32 feature strings"));
    }
    Ok(entries)
}

fn observe_fields<'a>(entries: &[&'a [u8]]) -> (Vec<u16>, Vec<Stat<'a>>) {
    let count = entries.len();
    let mut stats: Vec<Stat> = Vec::new();
    let mut field_counts: Vec<u16> = Vec::with_capacity(count);
    let mut fields = Vec::new();
    for (entry, text) in entries.iter().enumerate() {
        split_fields(text, &mut fields);
        field_counts.push(fields.len() as u16);
        while stats.len() < fields.len() {
            stats.push(Stat::new(count));
        }
        for (field, &(begin, end)) in fields.iter().enumerate() {
            stats[field].observe(entry, &text[begin..end]);
        }
    }
    (field_counts, stats)
}

fn classify_folds(stats: &mut [Stat], field_counts: &[u16]) -> Vec<Class> {
    let count = field_counts.len();
    let mut joint: Vec<u32> = {
        let mut known: SmallKeyMap<u16, u32> = SmallKeyMap::default();
        field_counts
            .iter()
            .map(|&number| {
                let next = known.len() as u32;
                *known.entry(number).or_insert(next)
            })
            .collect()
    };
    let mut order: Vec<usize> = (0..stats.len()).filter(|&f| stats[f].folds()).collect();
    order.sort_by_key(|&field| (stats[field].values.len(), field));
    let mut classes = vec![Class::Text; stats.len()];
    for field in order {
        let mut known: SmallKeyMap<(u32, u16), u32> = SmallKeyMap::default();
        let mut next = Vec::with_capacity(count);
        for (entry, &previous) in joint.iter().enumerate() {
            let length = known.len() as u32;
            next.push(
                *known
                    .entry((previous, stats[field].value_ids[entry]))
                    .or_insert(length),
            );
            if known.len() > MAX_TEMPLATES {
                break;
            }
        }
        if known.len() <= MAX_TEMPLATES {
            joint = next;
            classes[field] = Class::Folded;
        }
    }
    for stat in stats {
        stat.value_ids = Vec::new();
    }
    classes
}

struct Variants<'k> {
    list: Vec<(usize, &'k [u8])>,
    starts_of: Vec<u32>,
}

fn collect_variants<'k>(
    entries: &[&[u8]],
    tokens: &'k [(u32, &'k str)],
    planner: &Planner,
) -> Result<Variants<'k>, Error> {
    let mut planned: Vec<Option<TextOp>> = Vec::new();
    let mut variants: Vec<(usize, &[u8])> = Vec::new();
    let mut variant_of: SmallKeyMap<(u32, &[u8]), u32> = SmallKeyMap::default();
    let mut starts_of: Vec<u32> = Vec::with_capacity(tokens.len());
    let mut slots: Vec<&[u8]> = Vec::new();
    let mut fields = Vec::new();
    for &(feature, key) in tokens {
        let entry = feature as usize;
        let text = *entries.get(entry).ok_or(Error::Unsupported(
            "a token with a feature that is not there",
        ))?;
        let key = key.as_bytes();
        split_fields(text, &mut fields);
        planner.plan(entry, text, &fields, key, &mut slots, &mut planned);
        let keyed = planner.uses_key(&planned);
        let key: &[u8] = if keyed { key } else { b"" };
        let next = variants.len() as u32;
        let number = *variant_of.entry((feature, key)).or_insert(next);
        if number == next {
            variants.push((entry, key));
        }
        starts_of.push(number);
    }
    Ok(Variants {
        list: variants,
        starts_of,
    })
}

fn count_new_strings<'a>(
    entries: &[&'a [u8]],
    variants: &Variants,
    planner: &Planner<'_, 'a>,
) -> SmallKeyMap<&'a [u8], u32> {
    let mut string_uses: SmallKeyMap<&[u8], u32> = SmallKeyMap::default();
    let mut planned: Vec<Option<TextOp>> = Vec::new();
    let mut slots: Vec<&[u8]> = Vec::new();
    let mut fields = Vec::new();
    for &(entry, key) in &variants.list {
        let text = entries[entry];
        split_fields(text, &mut fields);
        planner.plan(entry, text, &fields, key, &mut slots, &mut planned);
        for (&(begin, end), op) in fields.iter().zip(&planned) {
            if let Some(TextOp::New) = op {
                *string_uses.entry(&text[begin..end]).or_insert(0) += 1;
            }
        }
    }
    string_uses
}

pub(in crate::packed) fn encode(features: &[u8], tokens: &[(u32, &str)]) -> Result<Encoded, Error> {
    let entries = split_entries(features)?;
    let (field_counts, mut stats) = observe_fields(&entries);
    let mut classes = classify_folds(&mut stats, &field_counts);
    let numbers = plan_numbers(&entries, &field_counts, &stats, &mut classes);
    let blob = plan_blob(
        &entries,
        &field_counts,
        &classes,
        &numbers.stored,
        &numbers.stored_values,
        &numbers.values,
    );
    let derive = Derive::learn(&entries, tokens, &classes, blob.as_ref());
    let planner = Planner {
        classes: &classes,
        derive: &derive,
        blob: blob.as_ref(),
    };
    let variants = collect_variants(&entries, tokens, &planner)?;
    let string_uses = count_new_strings(&entries, &variants, &planner);
    let mut builder = ProgramBuilder::new(&planner, &entries, &numbers, &string_uses);
    for &(entry, key) in &variants.list {
        builder.add_variant(entry, key)?;
    }
    let set = builder.set;
    let recoded = recode(&set.area, &set.references, &set.inline, blob.as_ref())?;
    if recoded.area.len() > u32::MAX as usize {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }
    let records = write_records(&set, &recoded, blob.as_ref(), &numbers.stored)?;
    dump_strings(&recoded);
    let bytes = Assembly {
        set: &set,
        recoded: &recoded,
        records: &records,
        stored: &numbers.stored,
        blob: blob.as_ref(),
        derive: &derive,
    }
    .build()?;
    let starts = variants
        .starts_of
        .iter()
        .map(|&variant| records.starts[variant as usize])
        .collect();
    Ok(Encoded { bytes, starts })
}
