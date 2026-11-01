mod parallel;
mod records;
mod strings;

use std::ops::Range;

use super::chars::{self, ENTRY_BYTES};
use super::pieces::{POSITION_BYTES, Position};
use super::{
    Counts, HEADER_BYTES, Layout, MAX_NUMERIC, MAX_RULE_SUFFIX, MAX_SLOTS, NumSpec, OP_BLOB,
    OP_BLOB_NEXT, OP_COPY, OP_COPY_MAX, OP_DERIVE, OP_EMPTY, OP_HIGH, OP_HIGH_MAX, OP_INLINE,
    OP_KEY, OP_NUM, OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF, OP_STAR, RULE_BYTES, RULE_CHAR_BITS,
    RULE_CHAR_MASK, RULE_HEADER_BYTES, SPEC_BYTES, read_bits,
};
use crate::Error;
use crate::packed::reader::Source;
use crate::wire::u32_values;

pub(in crate::packed) struct Checked {
    pub(in crate::packed) layout: Layout,
    pub(in crate::packed) specs: Vec<NumSpec>,
    starts: Vec<u64>,
    records: usize,
}

pub(in crate::packed) struct MappedLayout(Layout);

impl MappedLayout {
    #[inline(always)]
    pub(in crate::packed) fn layout(&self) -> &Layout {
        &self.0
    }
}

impl Checked {
    pub(in crate::packed) fn into_mapped(
        self,
        section: &Range<usize>,
        map_length: usize,
    ) -> Option<(MappedLayout, Vec<NumSpec>)> {
        let layout = self.layout.moved(section.start);
        (section.end <= map_length && layout.ends_within(section.end))
            .then_some((MappedLayout(layout), self.specs))
    }

    #[inline]
    pub(in crate::packed) fn is_record(&self, offset: usize) -> bool {
        offset < self.records
            && self
                .starts
                .get(offset >> 6)
                .is_some_and(|word| word >> (offset & 63) & 1 != 0)
    }
}
fn offsets_of(bytes: &[u8], blob_length: usize) -> Option<Vec<u32>> {
    let blob_length = u32::try_from(blob_length).ok()?;
    let offsets: Vec<u32> = u32_values(bytes).collect();
    let valid = offsets.first() == Some(&0)
        && offsets.last() == Some(&blob_length)
        && offsets.windows(2).all(|pair| pair[0] <= pair[1]);
    valid.then_some(offsets)
}

fn read_range(source: &Source, section: usize, range: &Range<usize>) -> Result<Vec<u8>, Error> {
    let mut bytes = vec![0; range.len()];
    source.read(section, range.start, &mut bytes)?;
    Ok(bytes)
}

struct ShapeTable {
    bytes: Vec<u8>,
    offsets: Vec<u32>,
}

impl ShapeTable {
    fn get(&self, shape: usize) -> Option<&[u8]> {
        let start = *self.offsets.get(shape)? as usize;
        let end = *self.offsets.get(shape + 1)? as usize;
        self.bytes.get(start..end)
    }
}

struct Tables<'a> {
    templates: usize,
    positions: usize,
    specs: &'a [NumSpec],
    string_boundaries: Vec<u64>,
    blob_starts: Vec<u64>,
    blob_bytes: usize,
    blob_bits: u32,
    strings: usize,
    chars: usize,
    ref_bits: u32,
    template_bits: u32,
    shape_bits: u32,
    shape_ops: ShapeTable,
}

pub(in crate::packed) fn check(source: &Source, section: usize) -> Result<Checked, Error> {
    let layout = read_layout(source, section)?;
    let counts = layout.counts;
    let specs = check_specs(source, section, &layout)?;
    check_blob_header(source, &layout, &specs)?;
    check_char_table(source, section, &layout)?;
    check_pieces(source, section, &layout)?;
    let shape_ops = check_shapes(source, section, &layout, &specs)?;
    check_rules(source, section, &layout)?;
    let string_boundaries = strings::check_strings(source, section, &layout)?;
    let blob_starts = strings::check_blob(source, section, &layout, &specs)?;
    let tables = Tables {
        templates: counts.templates as usize,
        positions: counts.positions as usize,
        specs: &specs,
        string_boundaries,
        blob_starts,
        blob_bytes: counts.blob_bytes as usize,
        blob_bits: layout.blob_bits,
        strings: counts.strings as usize,
        chars: counts.chars as usize,
        ref_bits: layout.ref_bits,
        template_bits: layout.template_bits,
        shape_bits: layout.shape_bits,
        shape_ops,
    };
    let starts = records::check_records(source, section, &layout, &tables)?;
    Ok(Checked {
        layout,
        specs,
        starts,
        records: counts.records as usize,
    })
}

fn read_layout(source: &Source, section: usize) -> Result<Layout, Error> {
    let length = source.section_length(section);
    if length < HEADER_BYTES {
        return Err(source.broken("the feature section is shorter than its header"));
    }
    let mut header = [0u8; HEADER_BYTES];
    source.read(section, 0, &mut header)?;
    Layout::parse(&header, length)
        .ok_or_else(|| source.broken("the feature section header is not valid"))
}

fn check_specs(source: &Source, section: usize, layout: &Layout) -> Result<Vec<NumSpec>, Error> {
    let spec_bytes = read_range(source, section, &layout.specs)?;
    spec_bytes
        .as_chunks::<SPEC_BYTES>()
        .0
        .iter()
        .map(|bytes| NumSpec::parse(bytes, layout.counts.lo_total))
        .collect::<Option<_>>()
        .ok_or_else(|| source.broken("a number of the features is not specified correctly"))
}

fn check_blob_header(source: &Source, layout: &Layout, specs: &[NumSpec]) -> Result<(), Error> {
    let counts = layout.counts;
    let blob_is_valid = counts.blob_fields == 0
        || specs
            .get(counts.blob_number as usize)
            .is_some_and(|spec| spec.hi_bits >= 1 && counts.id_bytes == spec.hi_bits.div_ceil(8));
    if blob_is_valid {
        Ok(())
    } else {
        Err(source.broken("the lemma table of the features is not valid"))
    }
}

fn check_char_table(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    let table = read_range(source, section, &layout.chars)?;
    let table_is_valid = table
        .as_chunks::<ENTRY_BYTES>()
        .0
        .iter()
        .all(|&entry| chars::entry_is_char(u64::from_le_bytes(entry)));
    if table_is_valid {
        Ok(())
    } else {
        Err(source.broken("the character table of the features is not valid"))
    }
}

struct ShapeScan {
    slots: usize,
    numbers: u8,
    blob_seen: u32,
}

impl ShapeScan {
    fn new() -> Self {
        Self {
            slots: 0,
            numbers: 0,
            blob_seen: 0,
        }
    }

    fn add_slot(&mut self) {
        self.slots = (self.slots + 1).min(MAX_SLOTS);
    }

    fn accepts(&mut self, op: u8, counts: &Counts, spec_count: usize) -> bool {
        match op {
            OP_REF | OP_INLINE => self.add_slot(),
            OP_BLOB if self.blob_seen == 0 && counts.blob_fields > 0 => {
                self.blob_seen = 1;
                self.add_slot();
            }
            OP_BLOB_NEXT if self.blob_seen > 0 && self.blob_seen < counts.blob_fields => {
                self.blob_seen += 1;
                self.add_slot();
            }
            OP_NUMB..=OP_NUMB_MAX
                if self.blob_seen > 0 && u32::from(op - OP_NUMB) == counts.blob_number =>
            {
                self.numbers |= 1 << (op - OP_NUMB);
            }
            OP_DERIVE if counts.rule_groups > 0 => self.add_slot(),
            OP_EMPTY | OP_STAR | OP_KEY => {}
            OP_COPY..=OP_COPY_MAX if usize::from(op - OP_COPY) < self.slots => {}
            OP_NUM..=OP_NUM_MAX if usize::from(op - OP_NUM) < spec_count => {
                self.numbers |= 1 << (op - OP_NUM);
            }
            OP_HIGH..=OP_HIGH_MAX
                if usize::from(op - OP_HIGH) < MAX_NUMERIC
                    && self.numbers >> (op - OP_HIGH) & 1 != 0 => {}
            _ => return false,
        }
        true
    }
}

fn check_shapes(
    source: &Source,
    section: usize,
    layout: &Layout,
    specs: &[NumSpec],
) -> Result<ShapeTable, Error> {
    let shape_bytes = read_range(source, section, &layout.shapes)?;
    let shape_offsets = offsets_of(
        &read_range(source, section, &layout.shape_offsets)?,
        shape_bytes.len(),
    )
    .ok_or_else(|| source.broken("the feature shapes are not delimited correctly"))?;
    for pair in shape_offsets.windows(2) {
        let ops = &shape_bytes[pair[0] as usize..pair[1] as usize];
        let mut scan = ShapeScan::new();
        if !ops
            .iter()
            .all(|&op| scan.accepts(op, &layout.counts, specs.len()))
        {
            return Err(source.broken("a feature shape has an operation that is not valid"));
        }
    }
    Ok(ShapeTable {
        bytes: shape_bytes,
        offsets: shape_offsets,
    })
}

fn words_of<const WIDTH: usize>(bytes: &[u8], read: fn([u8; WIDTH]) -> u32) -> Vec<u32> {
    bytes
        .as_chunks::<WIDTH>()
        .0
        .iter()
        .map(|&word| read(word))
        .collect()
}

fn rule_error(source: &Source) -> Error {
    source.broken("the derivation rules of the features are not valid")
}

fn check_rule_groups(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    let counts = layout.counts;
    let groups = counts.rule_groups as usize;
    let group_of_template = words_of::<2>(
        &read_range(source, section, &layout.template_groups)?,
        |word| u32::from(u16::from_le_bytes(word)),
    );
    if group_of_template
        .iter()
        .any(|&group| group as usize > groups)
    {
        return Err(rule_error(source));
    }
    let starts = words_of::<4>(
        &read_range(source, section, &layout.group_starts)?,
        u32::from_le_bytes,
    );
    let starts_are_valid = starts.len() == groups + 1
        && starts.first() == Some(&0)
        && starts
            .last()
            .is_some_and(|&last| last as usize == counts.rule_entries as usize)
        && starts.windows(2).all(|pair| pair[0] < pair[1]);
    if !starts_are_valid {
        return Err(rule_error(source));
    }
    let entries = words_of::<4>(
        &read_range(source, section, &layout.rule_entries)?,
        u32::from_le_bytes,
    );
    let rule_count = counts.rule_count;
    for pair in starts.windows(2) {
        let group = &entries[pair[0] as usize..pair[1] as usize];
        let entry_is_valid = |&entry: &u32| {
            char::from_u32(entry & RULE_CHAR_MASK).is_some() && entry >> RULE_CHAR_BITS < rule_count
        };
        let ascending = group
            .windows(2)
            .all(|pair| pair[0] & RULE_CHAR_MASK < pair[1] & RULE_CHAR_MASK);
        if !group.iter().all(entry_is_valid) || !ascending {
            return Err(rule_error(source));
        }
    }
    Ok(())
}

fn check_rule_texts(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    let counts = layout.counts;
    let rules = read_range(source, section, &layout.rules)?;
    debug_assert!(rules.len() >= RULE_BYTES * counts.rule_count as usize);
    let rules = &rules[..RULE_BYTES * counts.rule_count as usize];
    for rule in rules.as_chunks::<RULE_BYTES>().0 {
        let length = usize::from(rule[1]);
        let suffix_end = RULE_HEADER_BYTES + length;
        let is_valid = length <= MAX_RULE_SUFFIX
            && rule[suffix_end..].iter().all(|&byte| byte == 0)
            && str::from_utf8(&rule[RULE_HEADER_BYTES..suffix_end]).is_ok();
        if !is_valid {
            return Err(rule_error(source));
        }
    }
    Ok(())
}

fn check_rules(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    if layout.counts.rule_groups == 0 {
        return Ok(());
    }
    check_rule_groups(source, section, layout)?;
    check_rule_texts(source, section, layout)
}

fn check_pieces(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    let counts = layout.counts;
    let invalid = |reason: &'static str| source.broken(reason);
    let position_bytes = read_range(source, section, &layout.positions)?;
    let entry_bits = counts.entry_bits as usize;
    let positions: Vec<Position> = position_bytes
        .as_chunks::<POSITION_BYTES>()
        .0
        .iter()
        .map(Position::parse)
        .collect::<Option<_>>()
        .ok_or_else(|| invalid("a piece position of the features is not valid"))?;
    if positions.iter().any(|position| {
        !position.constant && position.shift as usize + position.bits as usize > entry_bits
    }) {
        return Err(invalid("a piece position of the features leaves its entry"));
    }
    let mut pool = read_range(source, section, &layout.pool)?;
    pool.truncate(counts.pool_bytes as usize);
    let offsets = offsets_of(
        &read_range(source, section, &layout.piece_offsets)?,
        pool.len(),
    )
    .ok_or_else(|| invalid("the feature pieces are not delimited correctly"))?;
    for pair in offsets.windows(2) {
        if str::from_utf8(&pool[pair[0] as usize..pair[1] as usize]).is_err() {
            return Err(invalid("a feature piece is not valid UTF-8"));
        }
    }
    for position in positions.iter().filter(|position| position.constant) {
        let start = position.base as usize;
        let piece = pool.get(start..start + position.shift as usize);
        if piece.is_none_or(|piece| str::from_utf8(piece).is_err()) {
            return Err(invalid("a constant feature piece is not valid"));
        }
    }
    drop(pool);
    let entries = read_range(source, section, &layout.entries)?;
    let pieces = u64::from(counts.pieces);
    for template in 0..counts.templates as usize {
        for position in positions.iter().filter(|position| !position.constant) {
            let local = read_bits(
                &entries,
                template * entry_bits + position.shift as usize,
                position.bits,
            );
            if u64::from(position.base) + local >= pieces {
                return Err(invalid(
                    "a feature template names a piece that does not exist",
                ));
            }
        }
    }
    Ok(())
}
