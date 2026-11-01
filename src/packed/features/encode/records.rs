use super::{
    BLOB_SYNC_EVERY, BitWriter, Blob, Counts, Derive, Error, Layout, OP_BLOB, OP_INLINE, OP_NUM,
    OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF, Pieces, ProgramSet, Recoded, SYNC_EVERY, Stored,
    bits_for,
};

pub(super) struct Records {
    pub(super) bytes: Vec<u8>,
    pub(super) starts: Vec<u32>,
}

fn writer_after(base: usize) -> BitWriter {
    let mut writer = BitWriter::default();
    writer.bytes.resize(base, 0);
    writer
}

struct RecordWriter<'a, 'p> {
    set: &'a ProgramSet<'p>,
    recoded: &'a Recoded,
    stored: &'a [Stored],
    blob_offsets: Vec<u32>,
    writer: BitWriter,
    base: usize,
    template_bits: u32,
    shape_bits: u32,
    ref_bits: u32,
    blob_bits: u32,
    next_reference: usize,
    next_inline: usize,
    next_number: usize,
    next_blob: usize,
}

impl<'a, 'p> RecordWriter<'a, 'p> {
    fn new(
        set: &'a ProgramSet<'p>,
        recoded: &'a Recoded,
        blob: Option<&Blob>,
        stored: &'a [Stored],
        base: usize,
    ) -> Self {
        let blob_offsets: Vec<u32> = set
            .blob_ids
            .iter()
            .map(|&id| {
                let blob = blob.expect("a blob id needs a blob");
                recoded.blob_starts[blob.entry_of(id)]
            })
            .collect();
        Self {
            set,
            recoded,
            stored,
            blob_offsets,
            writer: writer_after(base),
            base,
            template_bits: bits_for(set.template_list.len() as u64),
            shape_bits: bits_for(set.shape_list.len() as u64),
            ref_bits: bits_for(recoded.area.len() as u64),
            blob_bits: bits_for(recoded.blob.len() as u64),
            next_reference: 0,
            next_inline: 0,
            next_number: 0,
            next_blob: 0,
        }
    }

    fn next_number(&mut self) -> (u64, u32) {
        let number = self.set.numbers[self.next_number];
        self.next_number += 1;
        number
    }

    fn write_op(&mut self, op: u8) {
        match op {
            OP_REF => {
                let reference = self.recoded.references[self.next_reference];
                self.writer.put(u64::from(reference), self.ref_bits);
                self.next_reference += 1;
            }
            OP_INLINE => {
                self.writer
                    .put_bytes(&self.recoded.inline[self.next_inline]);
                self.next_inline += 1;
            }
            OP_BLOB => {
                let offset = self.blob_offsets[self.next_blob];
                self.writer.put(u64::from(offset), self.blob_bits);
                self.next_blob += 1;
            }
            OP_NUMB..=OP_NUMB_MAX => {
                let (_, index) = self.next_number();
                let spec = &self.stored[usize::from(op - OP_NUMB)].spec;
                self.writer.put(u64::from(index), spec.lo_bits);
            }
            OP_NUM..=OP_NUM_MAX => {
                let (high, index) = self.next_number();
                let spec = &self.stored[usize::from(op - OP_NUM)].spec;
                self.writer.put(high, spec.hi_bits);
                self.writer.put(u64::from(index), spec.lo_bits);
            }
            _ => {}
        }
    }

    fn write_entry(&mut self, entry: usize) -> Result<u32, Error> {
        let start = self.writer.bytes.len();
        let start_offset = u32::try_from(start - self.base)
            .map_err(|_| Error::Unsupported("feature records of more than 4 GiB"))?;
        self.writer.put(
            u64::from(self.set.entry_template[entry]),
            self.template_bits,
        );
        self.writer
            .put(u64::from(self.set.entry_shape[entry]), self.shape_bits);
        for &op in &self.set.shape_list[self.set.entry_shape[entry] as usize] {
            self.write_op(op);
        }
        self.writer.finish_record();
        if self.writer.bytes.len() == start {
            self.writer.bytes.push(0);
        }
        Ok(start_offset)
    }
}

pub(super) fn write_records(
    set: &ProgramSet,
    recoded: &Recoded,
    blob: Option<&Blob>,
    stored: &[Stored],
    base: usize,
) -> Result<Records, Error> {
    let variant_count = set.entry_template.len();
    let mut records = RecordWriter::new(set, recoded, blob, stored, base);
    let mut starts: Vec<u32> = Vec::with_capacity(variant_count);
    for entry in 0..variant_count {
        starts.push(records.write_entry(entry)?);
    }
    Ok(Records {
        bytes: records.writer.bytes,
        starts,
    })
}

pub(super) fn too_big() -> Error {
    Error::Unsupported("a feature table that does not fit the section")
}

pub(super) struct Assembly<'s, 'a> {
    pub(super) set: &'s ProgramSet<'a>,
    pub(super) recoded: &'s Recoded,
    pub(super) stored: &'s [Stored],
    pub(super) blob: Option<&'s Blob<'a>>,
    pub(super) derive: &'s Derive<'a>,
}

pub(in crate::packed::features) struct Parts {
    pieces: Pieces,
    shape_table: ShapeTable,
    low_values: Vec<u64>,
    pub(in crate::packed::features) records_start: usize,
}

pub(super) struct ShapeTable {
    pub(super) offsets: Vec<u32>,
    pub(super) bytes: Vec<u8>,
}

pub(super) fn shape_table(shape_list: &[Vec<u8>]) -> Result<ShapeTable, Error> {
    let mut offsets: Vec<u32> = vec![0];
    let mut bytes: Vec<u8> = Vec::new();
    for shape in shape_list {
        bytes.extend_from_slice(shape);
        offsets.push(u32::try_from(bytes.len()).map_err(|_| too_big())?);
    }
    Ok(ShapeTable { offsets, bytes })
}

impl Assembly<'_, '_> {
    pub(super) fn counts(
        &self,
        pieces: &Pieces,
        shape_table: &ShapeTable,
        low_count: usize,
        records_length: usize,
    ) -> Result<Counts, Error> {
        let (set, recoded, blob, derive) = (self.set, self.recoded, self.blob, self.derive);
        Ok(Counts {
            templates: set.template_list.len() as u32,
            shapes: set.shape_list.len() as u32,
            numerics: self.stored.len() as u32,
            lo_total: u32::try_from(low_count).map_err(|_| too_big())?,
            chars: recoded.table.len() as u32,
            pieces: u32::try_from(pieces.offsets.len() - 1).map_err(|_| too_big())?,
            positions: pieces.positions.len() as u32,
            entry_bits: pieces.entry_bits,
            pool_bytes: pieces.pool.len() as u64,
            shape_bytes: shape_table.bytes.len() as u64,
            strings: recoded.area.len() as u64,
            blob_bytes: recoded.blob.len() as u64,
            blob_entries: recoded.blob_starts.len() as u32,
            rule_groups: derive.groups(),
            rule_entries: derive.entries.len() as u32,
            rule_count: derive.rule_list.len() as u32,
            blob_fields: blob.map_or(0, |blob| blob.fields.len() as u32),
            blob_number: blob.map_or(0, |blob| blob.number as u32),
            id_bytes: blob.map_or(0, |blob| blob.id_bytes as u32),
            records: records_length as u64,
            record_count: set.entry_template.len() as u32,
        })
    }

    pub(super) fn prepare(&self) -> Result<Parts, Error> {
        let pieces = super::super::pieces::build(&self.set.template_list).ok_or_else(too_big)?;
        let shape_table = shape_table(&self.set.shape_list)?;
        let low_values: Vec<u64> = self
            .stored
            .iter()
            .flat_map(|source| source.low_table.iter().copied())
            .collect();
        let counts = self.counts(&pieces, &shape_table, low_values.len(), 0)?;
        let records_start = Layout::new(counts).ok_or_else(too_big)?.records.start;
        Ok(Parts {
            pieces,
            shape_table,
            low_values,
            records_start,
        })
    }

    pub(super) fn build(
        &self,
        parts: &Parts,
        mut bytes: Vec<u8>,
        record_starts: &[u32],
    ) -> Result<Vec<u8>, Error> {
        debug_assert!(bytes.len() >= parts.records_start);
        let records_length = bytes.len() - parts.records_start;
        let counts = self.counts(
            &parts.pieces,
            &parts.shape_table,
            parts.low_values.len(),
            records_length,
        )?;
        let layout = Layout::new(counts).ok_or_else(too_big)?;
        debug_assert_eq!(layout.records.start, parts.records_start);
        bytes.resize(layout.length(), 0);
        bytes[..super::super::HEADER_BYTES].copy_from_slice(&layout.header());
        self.put_pieces(&mut bytes, &layout, &parts.pieces, &parts.shape_table);
        self.put_numbers(&mut bytes, &layout, &parts.low_values, record_starts);
        self.put_strings(&mut bytes, &layout);
        self.put_rules(&mut bytes, &layout);
        Ok(bytes)
    }

    #[allow(
        clippy::unused_self,
        reason = "keeps the encoder method signature uniform"
    )]
    pub(super) fn put_pieces(
        &self,
        bytes: &mut [u8],
        layout: &Layout,
        pieces: &Pieces,
        shape_table: &ShapeTable,
    ) {
        let position_bytes: Vec<u8> = pieces
            .positions
            .iter()
            .flat_map(|position| position.to_bytes())
            .collect();
        put(bytes, &layout.positions, &position_bytes);
        put(bytes, &layout.piece_offsets, &words(&pieces.offsets));
        put(bytes, &layout.pool, &pieces.pool);
        put(bytes, &layout.entries, &pieces.entries);
        put(bytes, &layout.shape_offsets, &words(&shape_table.offsets));
        put(bytes, &layout.shapes, &shape_table.bytes);
    }

    pub(super) fn put_numbers(
        &self,
        bytes: &mut [u8],
        layout: &Layout,
        low_values: &[u64],
        record_starts: &[u32],
    ) {
        let spec_bytes: Vec<u8> = self
            .stored
            .iter()
            .flat_map(|source| source.spec.to_bytes())
            .collect();
        put(bytes, &layout.specs, &spec_bytes);
        let low_bytes: Vec<u8> = low_values.iter().flat_map(|v| v.to_le_bytes()).collect();
        put(bytes, &layout.lo, &low_bytes);
        let sync: Vec<u32> = record_starts.iter().step_by(SYNC_EVERY).copied().collect();
        put(bytes, &layout.sync, &words(&sync));
    }

    pub(super) fn put_strings(&self, bytes: &mut [u8], layout: &Layout) {
        let recoded = self.recoded;
        let table_bytes: Vec<u8> = recoded
            .table
            .iter()
            .flat_map(|entry| entry.to_le_bytes())
            .collect();
        put(bytes, &layout.chars, &table_bytes);
        put(bytes, &layout.strings, &recoded.area);
        let blob_sync: Vec<u32> = recoded
            .blob_starts
            .iter()
            .step_by(BLOB_SYNC_EVERY)
            .copied()
            .collect();
        put(bytes, &layout.blob_sync, &words(&blob_sync));
        put(bytes, &layout.blob, &recoded.blob);
    }

    pub(super) fn put_rules(&self, bytes: &mut [u8], layout: &Layout) {
        let derive = self.derive;
        if derive.groups() == 0 {
            return;
        }
        let group_bytes: Vec<u8> = self
            .set
            .template_groups
            .iter()
            .flat_map(|group| group.to_le_bytes())
            .collect();
        put(bytes, &layout.template_groups, &group_bytes);
        let mut group_starts = derive.group_starts.clone();
        group_starts.push(derive.entries.len() as u32);
        put(bytes, &layout.group_starts, &words(&group_starts));
        put(bytes, &layout.rule_entries, &words(&derive.entries));
        put(bytes, &layout.rules, &derive.rule_bytes());
    }
}

pub(super) fn put(bytes: &mut [u8], range: &std::ops::Range<usize>, data: &[u8]) {
    bytes[range.start..range.start + data.len()].copy_from_slice(data);
}

pub(super) fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
