use super::{
    Class, Error, FastMap, Numbers, OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_DERIVE, OP_EMPTY, OP_HIGH,
    OP_INLINE, OP_KEY, OP_NUM, OP_NUMB, OP_REF, OP_STAR, Planner, Spans, TextOp, blob_slot,
    plan_op, push_piece,
};

const NO_OFFSET: u32 = u32::MAX;

pub(super) fn intern_bytes(map: &mut FastMap<Vec<u8>, u32>, bytes: &[u8]) -> u32 {
    if let Some(&number) = map.get(bytes) {
        return number;
    }
    let number = map.len() as u32;
    map.insert(bytes.to_vec(), number);
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

pub(super) struct ProgramBuilder<'c, 'a> {
    pub(super) planner: &'c Planner<'c, 'a>,
    pub(super) entries: &'c [&'a [u8]],
    pub(super) spans: &'c Spans,
    pub(super) numbers: &'c Numbers,
    pub(super) uses_of_id: &'c [u32],
    pub(super) set: ProgramSet<'a>,
    pub(super) templates: FastMap<Vec<u8>, u32>,
    pub(super) shapes: FastMap<Vec<u8>, u32>,
    pub(super) offset_of_id: Vec<u32>,
    pub(super) fields: Vec<(usize, usize)>,
    pub(super) planned: Vec<Option<TextOp>>,
    pub(super) string_ids: Vec<u32>,
    pub(super) plan_codes: Vec<u8>,
    pub(super) plan_cursor: usize,
    pub(super) plan_begin: usize,
    pub(super) program: Vec<u8>,
    pub(super) piece: Vec<u8>,
    pub(super) ops: Vec<u8>,
}

impl<'c, 'a> ProgramBuilder<'c, 'a> {
    pub(super) fn new(
        planner: &'c Planner<'c, 'a>,
        entries: &'c [&'a [u8]],
        spans: &'c Spans,
        numbers: &'c Numbers,
        uses_of_id: &'c [u32],
        string_ids: Vec<u32>,
        plan_codes: Vec<u8>,
    ) -> Self {
        debug_assert_eq!(string_ids.len(), plan_codes.len());
        Self {
            planner,
            entries,
            spans,
            numbers,
            uses_of_id,
            set: ProgramSet::default(),
            templates: FastMap::default(),
            shapes: FastMap::default(),
            offset_of_id: vec![NO_OFFSET; uses_of_id.len()],
            fields: Vec::new(),
            planned: Vec::new(),
            string_ids,
            plan_codes,
            plan_cursor: 0,
            plan_begin: 0,
            program: Vec::new(),
            piece: Vec::new(),
            ops: Vec::new(),
        }
    }

    pub(super) fn finish(mut self) -> ProgramSet<'a> {
        debug_assert_eq!(self.plan_cursor, self.plan_codes.len());
        self.set.template_list = ordered_list(self.templates);
        self.set.shape_list = ordered_list(self.shapes);
        self.set
    }

    fn load_plan(&mut self) {
        let end = self.plan_cursor + self.fields.len();
        self.planned.clear();
        self.planned.extend(
            self.plan_codes[self.plan_cursor..end]
                .iter()
                .map(|&code| plan_op(code)),
        );
        self.plan_begin = self.plan_cursor;
        self.plan_cursor = end;
    }

    pub(super) fn add_variant(&mut self, entry: usize) -> Result<(), Error> {
        let text = self.entries[entry];
        self.spans.fill(entry, &mut self.fields);
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
            if self.planner.classes[field] == Class::Folded {
                self.piece.extend_from_slice(value);
                continue;
            }
            push_piece(&mut self.program, &self.piece);
            self.piece.clear();
            self.push_field_op(entry, field, value);
        }
        push_piece(&mut self.program, &self.piece);
        if self.set.area.len() > u32::MAX as usize {
            return Err(Error::Unsupported("feature strings of more than 4 GiB"));
        }
        let template = intern_bytes(&mut self.templates, &self.program);
        if template as usize == self.set.template_groups.len() {
            let group = self.planner.derive.table_of(text, &self.fields);
            self.set.template_groups.push(group);
        }
        self.set.entry_template.push(template);
        let shape = intern_bytes(&mut self.shapes, &self.ops);
        self.set.entry_shape.push(shape);
        Ok(())
    }

    pub(super) fn push_field_op(&mut self, entry: usize, field: usize, value: &'a [u8]) {
        match self.planner.classes[field] {
            Class::Folded => {}
            Class::Text => self.push_text_op(entry, field, value),
            Class::Number(number) => self.push_number_op(entry, number),
            Class::High(number) => self.ops.push(OP_HIGH + number as u8),
        }
    }

    pub(super) fn push_text_op(&mut self, entry: usize, field: usize, value: &'a [u8]) {
        match blob_slot(self.planner.blob, entry, field) {
            Some(0) => {
                self.ops.push(OP_BLOB);
                let number = self.planner.blob.expect("a blob field needs a blob").number;
                self.set.blob_ids.push(self.numbers.blob_id(number, entry));
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
                let id = self.string_ids[self.plan_begin + field] as usize;
                if self.uses_of_id[id] == 1 {
                    self.ops.push(OP_INLINE);
                    self.set.inline.push(value);
                } else {
                    self.ops.push(OP_REF);
                    let offset = self.area_offset(id, value);
                    self.set.references.push(offset);
                }
            }
        }
    }

    pub(super) fn area_offset(&mut self, id: usize, value: &'a [u8]) -> u32 {
        let known = self.offset_of_id[id];
        if known != NO_OFFSET {
            return known;
        }
        let offset = self.set.area.len() as u32;
        self.set.area.extend_from_slice(value);
        self.set.area.push(0);
        self.offset_of_id[id] = offset;
        offset
    }

    pub(super) fn push_number_op(&mut self, entry: usize, number: usize) {
        let from_blob = self
            .planner
            .blob
            .is_some_and(|blob| blob.number == number && blob.eligible[entry]);
        self.ops
            .push(if from_blob { OP_NUMB } else { OP_NUM } + number as u8);
        let stored = &self.numbers.stored[number];
        let value = self.numbers.value(number, entry);
        let low = value & ((1u64 << stored.spec.shift) - 1);
        let index = stored
            .low_table
            .binary_search(&low)
            .expect("every low part is in the table");
        self.set
            .numbers
            .push((value >> stored.spec.shift, index as u32));
    }
}
