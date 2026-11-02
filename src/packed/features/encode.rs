use super::{
    BLOB_SYNC_EVERY, BitWriter, Counts, Layout, MAX_BLOB_FIELDS, MAX_ID_BYTES, MAX_NUMERIC,
    MAX_RULE_SUFFIX, MAX_RULES, MAX_SLOTS, NumSpec, OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_DERIVE,
    OP_EMPTY, OP_HIGH, OP_INLINE, OP_KEY, OP_NUM, OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF,
    OP_STAR, RECORD_PADDING, RULE_BYTES, RULE_CHAR_BITS, RULE_HEADER_BYTES, SYNC_EVERY, bits_for,
    chars, pieces::Pieces, push_piece,
};
use crate::Error;

mod blob;
mod derive;
mod lanes;
mod numbers;
mod plans;
mod program;
mod records;
mod scan;
mod spans;
mod split;
mod stats;
mod tokens;
mod variants;

use crate::hash::FastMap;
use blob::{Blob, Recoded, plan_blob, recode};
use derive::{Derive, Planner, TextOp};
use lanes::for_each_lane;
use numbers::{Numbers, Stored, plan_numbers};
use plans::plan_op;
use program::{ProgramBuilder, ProgramSet};
use records::{Assembly, write_records};
use scan::parse_number;
use spans::Spans;
use split::split_entries;
use stats::{Class, Stat, choose_split};
use variants::{Variants, collect_variants};

pub(in crate::packed) use tokens::Tokens;

const MAX_FIELDS: usize = 256;
const FOLD_MAX_VALUES: usize = 256;
const MAX_TEMPLATES: usize = 1 << 16;
const MAX_LOW_PARTS: usize = 1 << 16;
const ABSENT: u16 = u16::MAX;
const NO_VARIANT: u32 = u32::MAX;
const NO_STRING: u32 = u32::MAX;
const DERIVE_TARGET: usize = 10;
const DERIVE_CONTEXT: [usize; 2] = [4, 5];
const MIN_RULE_USES: u32 = 2;
const MAX_REMOVED_CHARS: usize = 15;

pub(in crate::packed) struct Encoded {
    pub(in crate::packed) bytes: Vec<u8>,
    pub(in crate::packed) starts: Vec<u32>,
}

fn blob_slot(blob: Option<&Blob>, entry: usize, field: usize) -> Option<usize> {
    blob.and_then(|blob| blob.slot(entry, field))
}

fn observe_fields<'a>(entries: &[&'a [u8]], spans: &Spans, field_counts: &[u16]) -> Vec<Stat<'a>> {
    let count = entries.len();
    let width = field_counts.iter().copied().max().map_or(0, usize::from);
    let mut stats: Vec<Stat> = (0..width).map(|_| Stat::new()).collect();
    for_each_lane(&mut stats, count, |field, stat| {
        for (entry, &text) in entries.iter().enumerate() {
            if usize::from(field_counts[entry]) > field {
                let (begin, end) = spans.span(entry, field);
                stat.observe(entry, &text[begin..end]);
            }
        }
        stat.trim();
    });
    stats
}

struct JointIndex {
    id_plus_one: Vec<u32>,
    width: usize,
    len: u32,
}

impl JointIndex {
    fn new(joints: u32, values: usize) -> Self {
        let width = values + 1;
        Self {
            id_plus_one: vec![0u32; joints as usize * width],
            width,
            len: 0,
        }
    }

    fn id(&mut self, previous: u32, value: u16) -> u32 {
        let column = if value == ABSENT {
            self.width - 1
        } else {
            usize::from(value)
        };
        let cell = &mut self.id_plus_one[previous as usize * self.width + column];
        if *cell == 0 {
            self.len += 1;
            *cell = self.len;
        }
        *cell - 1
    }
}

fn classify_folds(stats: &mut [Stat], field_counts: &[u16]) -> Vec<Class> {
    let count = field_counts.len();
    let (mut joint, mut joints): (Vec<u32>, u32) = {
        let mut known = vec![u32::MAX; MAX_FIELDS + 1];
        let mut total = 0u32;
        let joint = field_counts
            .iter()
            .map(|&number| {
                let slot = &mut known[usize::from(number)];
                if *slot == u32::MAX {
                    *slot = total;
                    total += 1;
                }
                *slot
            })
            .collect();
        (joint, total)
    };
    let mut order: Vec<usize> = (0..stats.len()).filter(|&f| stats[f].folds()).collect();
    order.sort_by_key(|&field| (stats[field].values.len(), field));
    let mut classes = vec![Class::Text; stats.len()];
    for field in order {
        let mut known = JointIndex::new(joints, stats[field].values.len());
        let mut next = Vec::with_capacity(count);
        for (entry, &previous) in joint.iter().enumerate() {
            next.push(known.id(previous, stats[field].id_of(entry)));
            if known.len as usize > MAX_TEMPLATES {
                break;
            }
        }
        if known.len as usize <= MAX_TEMPLATES {
            joint = next;
            joints = known.len;
            classes[field] = Class::Folded;
        }
    }
    for stat in stats {
        stat.value_ids = Vec::new();
    }
    classes
}

fn classify_and_number(
    entries: &[&[u8]],
    spans: &Spans,
    field_counts: &[u16],
) -> (Vec<Class>, Numbers) {
    let mut stats = observe_fields(entries, spans, field_counts);
    let mut classes = classify_folds(&mut stats, field_counts);
    let numbers = plan_numbers(entries, spans, field_counts, &stats, &mut classes);
    (classes, numbers)
}

fn release_encoded_sources(set: &mut ProgramSet<'_>) {
    set.area = Vec::new();
    set.references = Vec::new();
    set.inline = Vec::new();
}

fn release_written_operands(set: &mut ProgramSet<'_>) {
    set.numbers = Vec::new();
    set.blob_ids = Vec::new();
    set.entry_shape = Vec::new();
}

fn assembly_of<'s, 'a>(
    set: &'s ProgramSet<'a>,
    recoded: &'s Recoded,
    stored: &'s [Stored],
    blob: Option<&'s Blob<'a>>,
    derive: &'s Derive<'a>,
) -> Assembly<'s, 'a> {
    Assembly {
        set,
        recoded,
        stored,
        blob,
        derive,
    }
}

pub(in crate::packed) fn encode(features: &[u8], tokens: Tokens<'_>) -> Result<Encoded, Error> {
    let entries = split_entries(features)?;
    let (spans, field_counts) = Spans::build(&entries);
    let (classes, numbers) = classify_and_number(&entries, &spans, &field_counts);
    let mut blob = plan_blob(
        &entries,
        &spans,
        &field_counts,
        &classes,
        &numbers.stored,
        &numbers.stored_values,
        &numbers.values,
    );
    drop(field_counts);
    let derive = Derive::learn(&entries, &spans, tokens, &classes, blob.as_ref())?;
    let planner = Planner {
        classes: &classes,
        derive: &derive,
        blob: blob.as_ref(),
    };
    let variants = collect_variants(&entries, &spans, tokens, &planner)?;
    let Variants {
        list,
        starts_of,
        uses_of_id,
        segments,
    } = variants;
    let mut builder =
        ProgramBuilder::new(&planner, &entries, &spans, &numbers, &uses_of_id, segments);
    builder.add_variants(&list)?;
    drop(list);
    let mut set = builder.finish();
    drop(uses_of_id);
    drop(spans);
    drop(entries);
    let stored = numbers.stored;
    drop(numbers.values);
    let recoded = recode(&set.area, &set.references, &set.inline, blob.as_ref())?;
    release_encoded_sources(&mut set);
    if let Some(blob) = blob.as_mut() {
        blob.release_texts();
    }
    if recoded.area.len() > u32::MAX as usize {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }
    let parts = assembly_of(&set, &recoded, &stored, blob.as_ref(), &derive).prepare()?;
    let records = write_records(&set, &recoded, blob.as_ref(), &stored, parts.records_start)?;
    release_written_operands(&mut set);
    let bytes = assembly_of(&set, &recoded, &stored, blob.as_ref(), &derive).build(
        &parts,
        records.bytes,
        &records.starts,
    )?;
    let starts = starts_of
        .iter()
        .map(|&variant| records.starts[variant as usize])
        .collect();
    Ok(Encoded { bytes, starts })
}
