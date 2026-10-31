use super::{
    BLOB_SYNC_EVERY, BitWriter, Blob, Counts, Derive, Error, Layout, OP_BLOB, OP_INLINE, OP_NUM,
    OP_NUM_MAX, OP_NUMB, OP_NUMB_MAX, OP_REF, Pieces, ProgramSet, Recoded, SYNC_EVERY, Stored,
    bits_for,
};

pub(super) struct Records {
    pub(super) bytes: Vec<u8>,
    pub(super) starts: Vec<u32>,
}

pub(super) fn write_records(
    set: &ProgramSet,
    recoded: &Recoded,
    blob: Option<&Blob>,
    stored: &[Stored],
) -> Result<Records, Error> {
    let variant_count = set.entry_template.len();
    let template_bits = bits_for(set.template_list.len() as u64);
    let shape_bits = bits_for(set.shape_list.len() as u64);
    let ref_bits = bits_for(recoded.area.len() as u64);
    let blob_bits = bits_for(recoded.blob.len() as u64);
    let blob_offsets: Vec<u32> = set
        .blob_ids
        .iter()
        .map(|&id| {
            let blob = blob.expect("a blob id needs a blob");
            recoded.blob_starts[blob.entry_of(id)]
        })
        .collect();
    let mut writer = BitWriter::default();
    let mut starts: Vec<u32> = Vec::with_capacity(variant_count);
    let (mut next_reference, mut next_inline, mut next_number, mut next_blob) = (0, 0, 0, 0);
    for entry in 0..variant_count {
        let start = writer.bytes.len();
        starts.push(
            u32::try_from(start)
                .map_err(|_| Error::Unsupported("feature records of more than 4 GiB"))?,
        );
        writer.put(u64::from(set.entry_template[entry]), template_bits);
        writer.put(u64::from(set.entry_shape[entry]), shape_bits);
        for &op in &set.shape_list[set.entry_shape[entry] as usize] {
            match op {
                OP_REF => {
                    writer.put(u64::from(recoded.references[next_reference]), ref_bits);
                    next_reference += 1;
                }
                OP_INLINE => {
                    writer.put_bytes(&recoded.inline[next_inline]);
                    next_inline += 1;
                }
                OP_BLOB => {
                    writer.put(u64::from(blob_offsets[next_blob]), blob_bits);
                    next_blob += 1;
                }
                OP_NUMB..=OP_NUMB_MAX => {
                    let (_, index) = set.numbers[next_number];
                    next_number += 1;
                    let spec = &stored[usize::from(op - OP_NUMB)].spec;
                    writer.put(u64::from(index), spec.lo_bits);
                }
                OP_NUM..=OP_NUM_MAX => {
                    let (high, index) = set.numbers[next_number];
                    next_number += 1;
                    let spec = &stored[usize::from(op - OP_NUM)].spec;
                    writer.put(high, spec.hi_bits);
                    writer.put(u64::from(index), spec.lo_bits);
                }
                _ => {}
            }
        }
        writer.finish_record();
        if writer.bytes.len() == start {
            writer.bytes.push(0);
        }
    }
    Ok(Records {
        bytes: writer.bytes,
        starts,
    })
}

pub(super) fn dump_strings(recoded: &Recoded) {
    if let Ok(path) = std::env::var("CHARCODE_DUMP") {
        let _ = std::fs::write(format!("{path}.area"), &recoded.area);
        let mut joined = Vec::new();
        for text in &recoded.inline {
            joined.extend_from_slice(text);
            joined.push(0);
        }
        let _ = std::fs::write(format!("{path}.inline"), &joined);
    }
}

pub(super) fn too_big() -> Error {
    Error::Unsupported("a feature table that does not fit the section")
}

pub(super) struct Assembly<'s, 'a> {
    pub(super) set: &'s ProgramSet<'a>,
    pub(super) recoded: &'s Recoded,
    pub(super) records: &'s Records,
    pub(super) stored: &'s [Stored],
    pub(super) blob: Option<&'s Blob<'a>>,
    pub(super) derive: &'s Derive<'a>,
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
            records: self.records.bytes.len() as u64,
            record_count: set.entry_template.len() as u32,
        })
    }

    pub(super) fn build(&self) -> Result<Vec<u8>, Error> {
        let pieces = super::super::pieces::build(&self.set.template_list).ok_or_else(too_big)?;
        let shape_table = shape_table(&self.set.shape_list)?;
        let low_values: Vec<u64> = self
            .stored
            .iter()
            .flat_map(|source| source.low_table.iter().copied())
            .collect();
        let counts = self.counts(&pieces, &shape_table, low_values.len())?;
        let layout = Layout::new(counts).ok_or_else(too_big)?;
        let mut bytes = vec![0u8; layout.length()];
        bytes[..super::super::HEADER_BYTES].copy_from_slice(&layout.header());
        self.put_pieces(&mut bytes, &layout, &pieces, &shape_table);
        self.put_numbers(&mut bytes, &layout, &low_values);
        self.put_strings(&mut bytes, &layout);
        self.put_rules(&mut bytes, &layout);
        put(&mut bytes, &layout.records, &self.records.bytes);
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

    pub(super) fn put_numbers(&self, bytes: &mut [u8], layout: &Layout, low_values: &[u64]) {
        let spec_bytes: Vec<u8> = self
            .stored
            .iter()
            .flat_map(|source| source.spec.to_bytes())
            .collect();
        put(bytes, &layout.specs, &spec_bytes);
        let low_bytes: Vec<u8> = low_values.iter().flat_map(|v| v.to_le_bytes()).collect();
        put(bytes, &layout.lo, &low_bytes);
        let sync: Vec<u32> = self
            .records
            .starts
            .iter()
            .step_by(SYNC_EVERY)
            .copied()
            .collect();
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
