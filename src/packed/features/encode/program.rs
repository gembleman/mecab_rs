use super::{
    Class, Error, Numbers, OP_BLOB, OP_BLOB_NEXT, OP_COPY, OP_DERIVE, OP_EMPTY, OP_HIGH, OP_INLINE,
    OP_KEY, OP_NUM, OP_NUMB, OP_REF, OP_STAR, Planner, SmallKeyMap, TextOp, blob_slot, push_piece,
    split_fields,
};

pub(super) fn intern_bytes(
    map: &mut SmallKeyMap<Vec<u8>, u32>,
    list: &mut Vec<Vec<u8>>,
    bytes: &[u8],
) -> u32 {
    if let Some(&number) = map.get(bytes) {
        return number;
    }
    let number = list.len() as u32;
    list.push(bytes.to_vec());
    map.insert(bytes.to_vec(), number);
    number
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
    pub(super) numbers: &'c Numbers,
    pub(super) string_uses: &'c SmallKeyMap<&'a [u8], u32>,
    pub(super) set: ProgramSet<'a>,
    pub(super) templates: SmallKeyMap<Vec<u8>, u32>,
    pub(super) shapes: SmallKeyMap<Vec<u8>, u32>,
    pub(super) known_strings: SmallKeyMap<&'a [u8], u32>,
    pub(super) fields: Vec<(usize, usize)>,
    pub(super) planned: Vec<Option<TextOp>>,
    pub(super) slots: Vec<&'a [u8]>,
    pub(super) program: Vec<u8>,
    pub(super) piece: Vec<u8>,
    pub(super) ops: Vec<u8>,
}

impl<'c, 'a> ProgramBuilder<'c, 'a> {
    pub(super) fn new(
        planner: &'c Planner<'c, 'a>,
        entries: &'c [&'a [u8]],
        numbers: &'c Numbers,
        string_uses: &'c SmallKeyMap<&'a [u8], u32>,
    ) -> Self {
        Self {
            planner,
            entries,
            numbers,
            string_uses,
            set: ProgramSet::default(),
            templates: SmallKeyMap::default(),
            shapes: SmallKeyMap::default(),
            known_strings: SmallKeyMap::default(),
            fields: Vec::new(),
            planned: Vec::new(),
            slots: Vec::new(),
            program: Vec::new(),
            piece: Vec::new(),
            ops: Vec::new(),
        }
    }

    pub(super) fn add_variant(&mut self, entry: usize, key: &[u8]) -> Result<(), Error> {
        let text = self.entries[entry];
        split_fields(text, &mut self.fields);
        self.program.clear();
        self.piece.clear();
        self.ops.clear();
        self.planner.plan(
            entry,
            text,
            &self.fields,
            key,
            &mut self.slots,
            &mut self.planned,
        );
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
        let template = intern_bytes(
            &mut self.templates,
            &mut self.set.template_list,
            &self.program,
        );
        if template as usize == self.set.template_groups.len() {
            let group = self.planner.derive.table_of(text, &self.fields);
            self.set.template_groups.push(group);
        }
        self.set.entry_template.push(template);
        let shape = intern_bytes(&mut self.shapes, &mut self.set.shape_list, &self.ops);
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
            TextOp::New if self.string_uses[value] == 1 => {
                self.ops.push(OP_INLINE);
                self.set.inline.push(value);
            }
            TextOp::New => {
                self.ops.push(OP_REF);
                let offset = self.area_offset(value);
                self.set.references.push(offset);
            }
        }
    }

    pub(super) fn area_offset(&mut self, value: &'a [u8]) -> u32 {
        let next = self.set.area.len();
        let offset = *self
            .known_strings
            .entry(value)
            .or_insert_with(|| next as u32);
        if offset as usize == next {
            self.set.area.extend_from_slice(value);
            self.set.area.push(0);
        }
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
