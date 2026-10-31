use super::{
    Class, MAX_NUMERIC, NumSpec, Stat, bits_for, choose_split, parse_number, split_fields,
};

pub(super) fn shifted_copy(
    earlier: &[u64],
    later: &[u64],
    field_counts: &[u16],
    first_field: usize,
    later_field: usize,
) -> Option<u32> {
    let has_both = |entry: usize| usize::from(field_counts[entry]) > first_field.max(later_field);
    let sample = (0..earlier.len()).find(|&entry| has_both(entry) && earlier[entry] != 0)?;
    (1..64u32).find(|&shift| {
        earlier[sample] >> shift == later[sample]
            && (0..earlier.len()).all(|entry| {
                usize::from(field_counts[entry]) <= later_field
                    || (has_both(entry) && later[entry] == earlier[entry] >> shift)
            })
    })
}

pub(super) struct Stored {
    pub(super) field: usize,
    pub(super) spec: NumSpec,
    pub(super) low_table: Vec<u64>,
}

#[derive(Default)]
pub(super) struct Numbers {
    pub(super) values: Vec<Vec<u64>>,
    pub(super) stored: Vec<Stored>,
    pub(super) stored_values: Vec<usize>,
}

impl Numbers {
    pub(super) fn value(&self, number: usize, entry: usize) -> u64 {
        self.values[self.stored_values[number]][entry]
    }

    pub(super) fn blob_id(&self, number: usize, entry: usize) -> u64 {
        self.value(number, entry) >> self.stored[number].spec.shift
    }
}

pub(super) fn parse_candidate_values(entries: &[&[u8]], candidates: &[usize]) -> Vec<Vec<u64>> {
    let mut values: Vec<Vec<u64>> = vec![vec![0; entries.len()]; candidates.len()];
    let mut fields = Vec::new();
    for (entry, text) in entries.iter().enumerate() {
        split_fields(text, &mut fields);
        for (slot, &field) in candidates.iter().enumerate() {
            if let Some(&(begin, end)) = fields.get(field) {
                values[slot][entry] = parse_number(&text[begin..end]).unwrap_or(0);
            }
        }
    }
    values
}

pub(super) fn high_source(
    numbers: &Numbers,
    slot: usize,
    field: usize,
    field_counts: &[u16],
) -> Option<usize> {
    let has = |entry: usize| usize::from(field_counts[entry]) > field;
    numbers
        .stored
        .iter()
        .enumerate()
        .position(|(number, source)| {
            let shift = source.spec.shift;
            (0..field_counts.len()).all(|entry| {
                !has(entry)
                    || (usize::from(field_counts[entry]) > source.field
                        && numbers.values[slot][entry] == numbers.value(number, entry) >> shift)
            })
        })
}

pub(super) fn split_number(
    values: &[Vec<u64>],
    candidates: &[usize],
    slot: usize,
    field_counts: &[u16],
    low_total: usize,
) -> Stored {
    let field = candidates[slot];
    let present: Vec<u64> = (0..field_counts.len())
        .filter(|&entry| usize::from(field_counts[entry]) > field)
        .map(|entry| values[slot][entry])
        .collect();
    let preferred_shift =
        candidates
            .iter()
            .enumerate()
            .skip(slot + 1)
            .find_map(|(other, &later)| {
                shifted_copy(&values[slot], &values[other], field_counts, field, later)
            });
    let (shift, low_table) = choose_split(&present, preferred_shift);
    let maximum = present.iter().copied().max().unwrap_or(0);
    let hi_bits = 64 - (maximum >> shift).leading_zeros();
    let spec = NumSpec {
        shift,
        hi_bits,
        lo_bits: bits_for(low_table.len() as u64),
        lo_start: low_total as u32,
        lo_count: low_table.len() as u32,
    };
    Stored {
        field,
        spec,
        low_table,
    }
}

pub(super) fn plan_numbers(
    entries: &[&[u8]],
    field_counts: &[u16],
    stats: &[Stat],
    classes: &mut [Class],
) -> Numbers {
    let candidates: Vec<usize> = (0..stats.len())
        .filter(|&f| classes[f] == Class::Text && stats[f].numeric && stats[f].present > 0)
        .collect();
    let mut numbers = Numbers {
        values: parse_candidate_values(entries, &candidates),
        ..Numbers::default()
    };
    let mut low_total = 0usize;
    for (slot, &field) in candidates.iter().enumerate() {
        if let Some(number) = high_source(&numbers, slot, field, field_counts) {
            classes[field] = Class::High(number);
            continue;
        }
        if numbers.stored.len() == MAX_NUMERIC {
            continue;
        }
        let stored = split_number(&numbers.values, &candidates, slot, field_counts, low_total);
        low_total += stored.low_table.len();
        classes[field] = Class::Number(numbers.stored.len());
        numbers.stored.push(stored);
        numbers.stored_values.push(slot);
    }
    numbers
}
