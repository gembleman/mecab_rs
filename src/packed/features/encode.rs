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
mod lanes;
mod numbers;
mod plans;
mod program;
mod records;
mod scan;
mod spans;
mod stats;

use crate::hash::FastMap;
use blob::{Blob, Recoded, plan_blob, recode};
use derive::{Derive, Planner, TextOp};
use lanes::for_each_lane;
use numbers::{Numbers, Stored, plan_numbers};
use plans::{plan_code, plan_op};
use program::{ProgramBuilder, ProgramSet};
use records::{Assembly, write_records};
use scan::parse_number;
use spans::Spans;
use stats::{Class, Stat, choose_split};

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

fn split_entries(features: &[u8]) -> Result<Vec<&[u8]>, Error> {
    if u32::try_from(features.len()).is_err() {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }
    let terminators: usize = features.iter().map(|&byte| usize::from(byte == 0)).sum();
    let mut entries: Vec<&[u8]> = Vec::with_capacity(terminators);
    let mut rest = features;
    while let Some(at) = rest.iter().position(|&byte| byte == 0) {
        entries.push(&rest[..at]);
        rest = &rest[at + 1..];
    }
    if u32::try_from(entries.len()).is_err() {
        return Err(Error::Unsupported("more than 2^32 feature strings"));
    }
    Ok(entries)
}

fn observe_fields<'a>(entries: &[&'a [u8]], spans: &Spans, field_counts: &[u16]) -> Vec<Stat<'a>> {
    let count = entries.len();
    let width = field_counts.iter().copied().max().map_or(0, usize::from);
    let mut stats: Vec<Stat> = (0..width).map(|_| Stat::new(count)).collect();
    for_each_lane(&mut stats, count, |field, stat| {
        for (entry, &text) in entries.iter().enumerate() {
            if usize::from(field_counts[entry]) > field {
                let (begin, end) = spans.span(entry, field);
                stat.observe(entry, &text[begin..end]);
            }
        }
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
            next.push(known.id(previous, stats[field].value_ids[entry]));
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

struct Variants<'k> {
    list: Vec<(usize, &'k [u8])>,
    starts_of: Vec<u32>,
    uses_of_id: Vec<u32>,
    string_ids: Vec<u32>,
    plan_codes: Vec<u8>,
}

#[derive(Default)]
struct StringIds<'a> {
    id_of: FastMap<&'a [u8], u32>,
    uses_of_id: Vec<u32>,
}

impl<'a> StringIds<'a> {
    fn count(&mut self, text: &'a [u8]) -> u32 {
        let next = self.uses_of_id.len() as u32;
        let id = *self.id_of.entry(text).or_insert(next);
        if id == next {
            self.uses_of_id.push(0);
        }
        self.uses_of_id[id as usize] += 1;
        id
    }
}

fn collect_variants<'k, 'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    tokens: &'k [(u32, &'k str)],
    planner: &Planner<'_, 'a>,
) -> Result<Variants<'k>, Error> {
    let mut planned: Vec<Option<TextOp>> = Vec::new();
    let mut variants: Vec<(usize, &[u8])> = Vec::new();
    let mut variant_of: FastMap<(u32, &[u8]), u32> = FastMap::default();
    let mut plain_variant: Vec<u32> = vec![NO_VARIANT; entries.len()];
    let mut filled_entry = usize::MAX;
    let mut starts_of: Vec<u32> = Vec::with_capacity(tokens.len());
    let mut string_table = StringIds::default();
    let mut string_ids: Vec<u32> = Vec::new();
    let mut plan_codes: Vec<u8> = Vec::new();
    let mut slots: Vec<&[u8]> = Vec::new();
    let mut fields = Vec::new();
    for &(feature, key) in tokens {
        let entry = feature as usize;
        let text = *entries.get(entry).ok_or(Error::Unsupported(
            "a token with a feature that is not there",
        ))?;
        let key = key.as_bytes();
        if filled_entry != entry {
            spans.fill(entry, &mut fields);
            filled_entry = entry;
        }
        planner.plan(entry, text, &fields, key, &mut slots, &mut planned);
        let keyed = planner.uses_key(&planned);
        let key: &[u8] = if keyed { key } else { b"" };
        let next = variants.len() as u32;
        let number = if key.is_empty() {
            let slot = &mut plain_variant[entry];
            if *slot == NO_VARIANT {
                *slot = next;
            }
            *slot
        } else {
            *variant_of.entry((feature, key)).or_insert(next)
        };
        if number == next {
            variants.push((entry, key));
            record_plan(
                text,
                &fields,
                &planned,
                &mut string_table,
                &mut string_ids,
                &mut plan_codes,
            );
        }
        starts_of.push(number);
    }
    Ok(Variants {
        list: variants,
        starts_of,
        uses_of_id: string_table.uses_of_id,
        string_ids,
        plan_codes,
    })
}

fn record_plan<'a>(
    text: &'a [u8],
    fields: &[(usize, usize)],
    planned: &[Option<TextOp>],
    string_table: &mut StringIds<'a>,
    string_ids: &mut Vec<u32>,
    plan_codes: &mut Vec<u8>,
) {
    for (&(begin, end), &op) in fields.iter().zip(planned) {
        let id = if let Some(TextOp::New) = op {
            string_table.count(&text[begin..end])
        } else {
            NO_STRING
        };
        string_ids.push(id);
        plan_codes.push(plan_code(op));
    }
}

pub(in crate::packed) fn encode(features: &[u8], tokens: &[(u32, &str)]) -> Result<Encoded, Error> {
    let entries = split_entries(features)?;
    let (spans, field_counts) = Spans::build(&entries);
    let mut stats = observe_fields(&entries, &spans, &field_counts);
    let mut classes = classify_folds(&mut stats, &field_counts);
    let numbers = plan_numbers(&entries, &spans, &field_counts, &stats, &mut classes);
    let blob = plan_blob(
        &entries,
        &spans,
        &field_counts,
        &classes,
        &numbers.stored,
        &numbers.stored_values,
        &numbers.values,
    );
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
        string_ids,
        plan_codes,
    } = variants;
    let mut builder = ProgramBuilder::new(
        &planner,
        &entries,
        &spans,
        &numbers,
        &uses_of_id,
        string_ids,
        plan_codes,
    );
    for &(entry, _) in &list {
        builder.add_variant(entry)?;
    }
    let set = builder.finish();
    let recoded = recode(&set.area, &set.references, &set.inline, blob.as_ref())?;
    if recoded.area.len() > u32::MAX as usize {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }
    let assembly = Assembly {
        set: &set,
        recoded: &recoded,
        stored: &numbers.stored,
        blob: blob.as_ref(),
        derive: &derive,
    };
    let parts = assembly.prepare()?;
    let records = write_records(
        &set,
        &recoded,
        blob.as_ref(),
        &numbers.stored,
        parts.records_start,
    )?;
    let bytes = assembly.build(&parts, records.bytes, &records.starts)?;
    let starts = starts_of
        .iter()
        .map(|&variant| records.starts[variant as usize])
        .collect();
    Ok(Encoded { bytes, starts })
}
