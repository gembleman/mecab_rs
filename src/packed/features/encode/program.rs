use std::{mem, thread};

use super::lanes::lane_count;
use super::plans::is_new_code;
use super::variants::Segment;
use super::{
    Class, Error, FastMap, Numbers, OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_DERIVE, OP_EMPTY, OP_HIGH,
    OP_INLINE, OP_KEY, OP_NUM, OP_NUMB, OP_REF, OP_STAR, Planner, Spans, TextOp, blob_slot,
    plan_op, push_piece,
};

const NO_LOCAL_NUMBER: u32 = u32::MAX;
const NO_AREA_STRING: u32 = u32::MAX;
const CHUNK_VARIANTS: usize = 1 << 16;
const SINGLE_USE: u32 = 1;

type PlannedPart<'p> = (&'p [u32], usize, usize);

pub(super) fn intern_bytes(map: &mut FastMap<Vec<u8>, u32>, bytes: &[u8]) -> u32 {
    if let Some(&number) = map.get(bytes) {
        return number;
    }
    let number = map.len() as u32;
    map.insert(bytes.to_vec(), number);
    number
}

fn intern_owned(map: &mut FastMap<Vec<u8>, u32>, bytes: &mut Vec<u8>) -> u32 {
    if let Some(&number) = map.get(bytes.as_slice()) {
        return number;
    }
    let number = map.len() as u32;
    map.insert(std::mem::take(bytes), number);
    number
}

fn ordered_list(map: FastMap<Vec<u8>, u32>) -> Vec<Vec<u8>> {
    let mut list = vec![Vec::new(); map.len()];
    for (bytes, number) in map {
        list[number as usize] = bytes;
    }
    list
}

#[derive(Default)]
pub(super) struct ProgramSet<'a> {
    pub(super) template_list: Vec<Vec<u8>>,
    pub(super) shape_list: Vec<Vec<u8>>,
    pub(super) area: Vec<u8>,
    pub(super) entry_template: Vec<u32>,
    pub(super) entry_shape: Vec<u32>,
    pub(super) references: Vec<u32>,
    pub(super) inline: Vec<&'a [u8]>,
    pub(super) template_groups: Vec<u16>,
    pub(super) numbers: Vec<(u64, u32)>,
    pub(super) blob_ids: Vec<u64>,
}

struct VariantChunk<'a> {
    template_list: Vec<Vec<u8>>,
    template_groups: Vec<u16>,
    shape_list: Vec<Vec<u8>>,
    entry_template: Vec<u32>,
    entry_shape: Vec<u32>,
    references: Vec<(u32, &'a [u8])>,
    inline: Vec<&'a [u8]>,
    numbers: Vec<(u64, u32)>,
    blob_ids: Vec<u64>,
}

impl VariantChunk<'_> {
    fn with_capacity(variants: usize) -> Self {
        Self {
            template_list: Vec::new(),
            template_groups: Vec::new(),
            shape_list: Vec::new(),
            entry_template: Vec::with_capacity(variants),
            entry_shape: Vec::with_capacity(variants),
            references: Vec::new(),
            inline: Vec::new(),
            numbers: Vec::new(),
            blob_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct Shared<'c, 'a> {
    planner: &'c Planner<'c, 'a>,
    entries: &'c [&'a [u8]],
    spans: &'c Spans,
    numbers: &'c Numbers,
    uses_of_id: &'c [u32],
    string_ids: &'c [u32],
    plan_codes: &'c [u8],
}

struct ChunkEncoder<'c, 'a> {
    shared: Shared<'c, 'a>,
    out: VariantChunk<'a>,
    templates: FastMap<Vec<u8>, u32>,
    shapes: FastMap<Vec<u8>, u32>,
    fields: Vec<(usize, usize)>,
    planned: Vec<Option<TextOp>>,
    plan_cursor: usize,
    id_cursor: usize,
    ids: Vec<u32>,
    program: Vec<u8>,
    piece: Vec<u8>,
    ops: Vec<u8>,
}

fn encode_chunk<'a>(
    shared: Shared<'_, 'a>,
    part: &[u32],
    plan_start: usize,
    id_start: usize,
) -> VariantChunk<'a> {
    let mut encoder = ChunkEncoder::new(shared, plan_start, id_start, part.len());
    for &entry in part {
        encoder.add_variant(entry as usize);
    }
    encoder.finish()
}

fn encode_chunks<'a>(shared: Shared<'_, 'a>, parts: Vec<PlannedPart<'_>>) -> Vec<VariantChunk<'a>> {
    if parts.len() == 1 {
        let (part, plan_start, id_start) = parts[0];
        return vec![encode_chunk(shared, part, plan_start, id_start)];
    }
    thread::scope(|scope| {
        let handles: Vec<_> = parts
            .into_iter()
            .map(|(part, plan_start, id_start)| {
                scope.spawn(move || encode_chunk(shared, part, plan_start, id_start))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a variant encoder panicked"))
            .collect()
    })
}

impl<'c, 'a> ChunkEncoder<'c, 'a> {
    fn new(shared: Shared<'c, 'a>, plan_start: usize, id_start: usize, variants: usize) -> Self {
        Self {
            shared,
            out: VariantChunk::with_capacity(variants),
            templates: FastMap::default(),
            shapes: FastMap::default(),
            fields: Vec::new(),
            planned: Vec::new(),
            plan_cursor: plan_start,
            id_cursor: id_start,
            ids: Vec::new(),
            program: Vec::new(),
            piece: Vec::new(),
            ops: Vec::new(),
        }
    }

    fn finish(mut self) -> VariantChunk<'a> {
        self.out.template_list = ordered_list(self.templates);
        self.out.shape_list = ordered_list(self.shapes);
        self.out
    }

    fn load_plan(&mut self) {
        let end = self.plan_cursor + self.fields.len();
        self.planned.clear();
        self.planned.extend(
            self.shared.plan_codes[self.plan_cursor..end]
                .iter()
                .map(|&code| plan_op(code)),
        );
        self.plan_cursor = end;
        self.load_ids();
    }

    fn load_ids(&mut self) {
        self.ids.clear();
        for &op in &self.planned {
            if let Some(TextOp::New) = op {
                self.ids.push(self.shared.string_ids[self.id_cursor]);
                self.id_cursor += 1;
            } else {
                self.ids.push(NO_AREA_STRING);
            }
        }
    }

    fn add_variant(&mut self, entry: usize) {
        let text = self.shared.entries[entry];
        self.shared.spans.fill(entry, &mut self.fields);
        self.program.clear();
        self.piece.clear();
        self.ops.clear();
        self.load_plan();
        for field in 0..self.fields.len() {
            let (begin, end) = self.fields[field];
            let value = &text[begin..end];
            if field > 0 {
                self.piece.push(b',');
            }
            if self.shared.planner.classes[field] == Class::Folded {
                self.piece.extend_from_slice(value);
                continue;
            }
            push_piece(&mut self.program, &self.piece);
            self.piece.clear();
            self.push_field_op(entry, field, value);
        }
        push_piece(&mut self.program, &self.piece);
        let template = intern_bytes(&mut self.templates, &self.program);
        if template as usize == self.out.template_groups.len() {
            let group = self.shared.planner.derive.table_of(text, &self.fields);
            self.out.template_groups.push(group);
        }
        self.out.entry_template.push(template);
        let shape = intern_bytes(&mut self.shapes, &self.ops);
        self.out.entry_shape.push(shape);
    }

    fn push_field_op(&mut self, entry: usize, field: usize, value: &'a [u8]) {
        match self.shared.planner.classes[field] {
            Class::Folded => {}
            Class::Text => self.push_text_op(entry, field, value),
            Class::Number(number) => self.push_number_op(entry, number),
            Class::High(number) => self.ops.push(OP_HIGH + number as u8),
        }
    }

    fn push_text_op(&mut self, entry: usize, field: usize, value: &'a [u8]) {
        let planner = self.shared.planner;
        match blob_slot(planner.blob, entry, field) {
            Some(0) => {
                self.ops.push(OP_BLOB);
                let number = planner.blob.expect("a blob field needs a blob").number;
                self.out
                    .blob_ids
                    .push(self.shared.numbers.blob_id(number, entry));
                return;
            }
            Some(_) => {
                self.ops.push(OP_BLOB_NEXT);
                return;
            }
            None => {}
        }
        match self.planned[field].expect("a text field has an operation") {
            TextOp::Key => self.ops.push(OP_KEY),
            TextOp::Star => self.ops.push(OP_STAR),
            TextOp::Empty => self.ops.push(OP_EMPTY),
            TextOp::Copy(slot) => self.ops.push(OP_COPY + slot),
            TextOp::Derive => self.ops.push(OP_DERIVE),
            TextOp::New => {
                let id = self.ids[field];
                if self.shared.uses_of_id[id as usize] == SINGLE_USE {
                    self.ops.push(OP_INLINE);
                    self.out.inline.push(value);
                } else {
                    self.ops.push(OP_REF);
                    self.out.references.push((id, value));
                }
            }
        }
    }

    fn push_number_op(&mut self, entry: usize, number: usize) {
        let from_blob = self
            .shared
            .planner
            .blob
            .is_some_and(|blob| blob.number == number && blob.eligible[entry]);
        self.ops
            .push(if from_blob { OP_NUMB } else { OP_NUM } + number as u8);
        let stored = &self.shared.numbers.stored[number];
        let value = self.shared.numbers.value(number, entry);
        let low = value & ((1u64 << stored.spec.shift) - 1);
        let index = stored
            .low_table
            .binary_search(&low)
            .expect("every low part is in the table");
        self.out
            .numbers
            .push((value >> stored.spec.shift, index as u32));
    }
}

fn global_number(
    local_to_global: &mut [u32],
    local: u32,
    local_list: &mut [Vec<u8>],
    global: &mut FastMap<Vec<u8>, u32>,
) -> u32 {
    let known = local_to_global[local as usize];
    if known != NO_LOCAL_NUMBER {
        return known;
    }
    let number = intern_owned(global, &mut local_list[local as usize]);
    local_to_global[local as usize] = number;
    number
}

#[derive(Default)]
struct Merge<'a> {
    set: ProgramSet<'a>,
    templates: FastMap<Vec<u8>, u32>,
    shapes: FastMap<Vec<u8>, u32>,
    area_string_of_id: Vec<u32>,
    area_strings: u32,
}

impl<'a> Merge<'a> {
    fn new(string_count: usize) -> Self {
        Self {
            area_string_of_id: vec![NO_AREA_STRING; string_count],
            ..Self::default()
        }
    }

    fn reserve_variants(&mut self, variants: usize) {
        self.set.entry_template.reserve_exact(variants);
        self.set.entry_shape.reserve_exact(variants);
    }

    fn absorb(&mut self, mut chunk: VariantChunk<'a>) -> Result<(), Error> {
        self.absorb_variants(&mut chunk);
        self.absorb_references(&chunk.references)?;
        self.set.inline.append(&mut chunk.inline);
        self.set.numbers.append(&mut chunk.numbers);
        self.set.blob_ids.append(&mut chunk.blob_ids);
        Ok(())
    }

    fn absorb_variants(&mut self, chunk: &mut VariantChunk<'a>) {
        let mut template_numbers = vec![NO_LOCAL_NUMBER; chunk.template_list.len()];
        let mut shape_numbers = vec![NO_LOCAL_NUMBER; chunk.shape_list.len()];
        self.set.entry_template.reserve(chunk.entry_template.len());
        self.set.entry_shape.reserve(chunk.entry_shape.len());
        let variants = chunk.entry_template.iter().zip(&chunk.entry_shape);
        for (&local_template, &local_shape) in variants {
            let known_templates = self.templates.len();
            let template = global_number(
                &mut template_numbers,
                local_template,
                &mut chunk.template_list,
                &mut self.templates,
            );
            if self.templates.len() > known_templates {
                self.set
                    .template_groups
                    .push(chunk.template_groups[local_template as usize]);
            }
            self.set.entry_template.push(template);
            let shape = global_number(
                &mut shape_numbers,
                local_shape,
                &mut chunk.shape_list,
                &mut self.shapes,
            );
            self.set.entry_shape.push(shape);
        }
    }

    fn absorb_references(&mut self, references: &[(u32, &'a [u8])]) -> Result<(), Error> {
        self.set.references.reserve(references.len());
        for &(id, value) in references {
            let string = self.area_string(id as usize, value)?;
            self.set.references.push(string);
        }
        Ok(())
    }

    fn area_string(&mut self, id: usize, value: &'a [u8]) -> Result<u32, Error> {
        let known = self.area_string_of_id[id];
        if known != NO_AREA_STRING {
            return Ok(known);
        }
        if self.set.area.len() > u32::MAX as usize {
            return Err(Error::Unsupported("feature strings of more than 4 GiB"));
        }
        let string = self.area_strings;
        self.area_strings += 1;
        self.set.area.extend_from_slice(value);
        self.set.area.push(0);
        self.area_string_of_id[id] = string;
        Ok(string)
    }

    fn finish(mut self) -> ProgramSet<'a> {
        self.set.template_list = ordered_list(self.templates);
        self.set.shape_list = ordered_list(self.shapes);
        self.set
    }
}

#[derive(Default)]
struct Cursor {
    plan: usize,
    id: usize,
}

fn variants_per_chunk(variants: usize, lanes: usize) -> usize {
    if lanes == 1 {
        variants.max(1)
    } else {
        variants.div_ceil(lanes).clamp(1, CHUNK_VARIANTS)
    }
}

fn plan_parts<'l>(
    spans: &Spans,
    plan_codes: &[u8],
    cursor: &mut Cursor,
    batch: &'l [u32],
    per_chunk: usize,
) -> Vec<PlannedPart<'l>> {
    batch
        .chunks(per_chunk)
        .map(|part| {
            let start = cursor.plan;
            let fields: usize = part
                .iter()
                .map(|&entry| spans.field_count(entry as usize))
                .sum();
            let new_strings = plan_codes[start..start + fields]
                .iter()
                .filter(|&&code| is_new_code(code))
                .count();
            let id_start = cursor.id;
            cursor.plan += fields;
            cursor.id += new_strings;
            (part, start, id_start)
        })
        .collect()
}

pub(super) struct ProgramBuilder<'c, 'a> {
    planner: &'c Planner<'c, 'a>,
    entries: &'c [&'a [u8]],
    spans: &'c Spans,
    numbers: &'c Numbers,
    uses_of_id: &'c [u32],
    segments: Vec<Segment>,
    merge: Merge<'a>,
}

impl<'c, 'a> ProgramBuilder<'c, 'a> {
    pub(super) fn new(
        planner: &'c Planner<'c, 'a>,
        entries: &'c [&'a [u8]],
        spans: &'c Spans,
        numbers: &'c Numbers,
        uses_of_id: &'c [u32],
        segments: Vec<Segment>,
    ) -> Self {
        Self {
            planner,
            entries,
            spans,
            numbers,
            uses_of_id,
            segments,
            merge: Merge::new(uses_of_id.len()),
        }
    }

    pub(super) fn finish(self) -> ProgramSet<'a> {
        debug_assert!(self.segments.is_empty());
        self.merge.finish()
    }

    pub(super) fn add_variants(&mut self, list: &[u32]) -> Result<(), Error> {
        let lanes = lane_count(list.len());
        self.merge.reserve_variants(list.len());
        let mut first = 0usize;
        for segment in mem::take(&mut self.segments) {
            let end = first + segment.variants;
            self.add_segment(&list[first..end], &segment, lanes)?;
            first = end;
        }
        debug_assert_eq!(first, list.len());
        Ok(())
    }

    fn add_segment(&mut self, list: &[u32], segment: &Segment, lanes: usize) -> Result<(), Error> {
        let per_chunk = variants_per_chunk(list.len(), lanes);
        let mut cursor = Cursor::default();
        for batch in list.chunks(per_chunk * lanes) {
            let parts = plan_parts(
                self.spans,
                &segment.plan_codes,
                &mut cursor,
                batch,
                per_chunk,
            );
            let shared = Shared {
                planner: self.planner,
                entries: self.entries,
                spans: self.spans,
                numbers: self.numbers,
                uses_of_id: self.uses_of_id,
                string_ids: &segment.string_ids,
                plan_codes: &segment.plan_codes,
            };
            for chunk in encode_chunks(shared, parts) {
                self.merge.absorb(chunk)?;
            }
        }
        debug_assert_eq!(cursor.plan, segment.plan_codes.len());
        debug_assert_eq!(cursor.id, segment.string_ids.len());
        Ok(())
    }
}
