use std::mem::size_of;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};

use crate::clusters::ClusterShape;
use crate::dictionary::map_file;
use crate::error::Error;
use crate::hot_shape::{CELL_SUM_BOUND, HotShape, MAX_KEYS};
use crate::int_templates::{
    MAGIC, MAX_COST_FACTOR, MAX_SHIFT, MappedIntTemplates, RuntimeInputs, int_cost,
};
use crate::mapped_bytes::MappedBytes;
use crate::section::{Section, SectionSource};
use crate::templates::{TemplateReader, write_array};

pub(crate) const RUNTIME_VERSION: u32 = 2;
const MAX_SHAPES: usize = 256;
const TABLE_SLACK: usize = 4;
const BLOCK_CELLS: usize = 64;
const BLOCK_BYTES: usize = 12;
const MIN_SPARSE_GAIN: usize = 1 << 18;
const ABOVE: u16 = 1 << 15;

struct Renumbered {
    new_of_old: Vec<u16>,
    old_of_new: Vec<u16>,
    mixed: usize,
}

fn renumber(keys: impl Iterator<Item = u16>, count: usize, hot: usize) -> Renumbered {
    let mut mixed = vec![false; count];
    for key in keys.skip(hot) {
        mixed[usize::from(key)] = true;
    }
    let mut old_of_new: Vec<u16> = (0..count as u16)
        .filter(|&key| mixed[usize::from(key)])
        .collect();
    let mixed_count = old_of_new.len();
    old_of_new.extend((0..count as u16).filter(|&key| !mixed[usize::from(key)]));
    let mut new_of_old = vec![0u16; count];
    for (new, &old) in old_of_new.iter().enumerate() {
        new_of_old[usize::from(old)] = new as u16;
    }
    Renumbered {
        new_of_old,
        old_of_new,
        mixed: mixed_count,
    }
}

impl MappedIntTemplates<Mmap> {
    pub fn runtime_form(&self, shape: &HotShape) -> Result<Vec<u8>, Error> {
        let inputs = self.runtime_inputs().ok_or(Error::Unsupported(
            "integer templates without clusters have no runtime form",
        ))?;
        runtime_form_from(&inputs, shape)
    }
}

pub(crate) fn runtime_form_from(
    inputs: &RuntimeInputs,
    shape: &HotShape,
) -> Result<Vec<u8>, Error> {
    write_runtime(inputs, shape, MIN_SPARSE_GAIN)
}

pub(crate) fn runtime_form_of(bytes: &[u8], shape: &HotShape) -> Result<Vec<u8>, Error> {
    let path = Path::new("matrix.itp");
    let io_error = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut map = MmapOptions::new()
        .len(bytes.len().max(1))
        .map_anon()
        .map_err(io_error)?;
    map[..bytes.len()].copy_from_slice(bytes);
    let map = map.make_read_only().map_err(io_error)?;
    MappedIntTemplates::from_source(map, path)?.runtime_form(shape)
}

fn cluster_starts(inputs: &RuntimeInputs) -> Vec<usize> {
    inputs
        .shapes
        .iter()
        .scan(0, |start, shape| {
            let at = *start;
            *start += shape.left_keys * shape.right_keys;
            Some(at)
        })
        .collect()
}

fn computed_total(inputs: &RuntimeInputs, starts: &[usize], row: usize, column: usize) -> i64 {
    let count = inputs.shapes.len();
    inputs
        .shapes
        .iter()
        .zip(starts)
        .enumerate()
        .map(|(cluster, (shape, &start))| {
            let left = usize::from(inputs.left_keys[column * count + cluster]);
            let right = usize::from(inputs.right_keys[row * count + cluster]);
            i64::from(inputs.tables[start + right * shape.left_keys + left])
        })
        .sum()
}

struct ExceptionColumns {
    row_starts: Vec<u32>,
    columns: Vec<u16>,
}

fn exceptions_outside_hot(
    inputs: &RuntimeInputs,
    shape: &HotShape,
    starts: &[usize],
) -> Result<ExceptionColumns, Error> {
    let (left_size, right_size) = (inputs.left_size, inputs.right_size);
    let mut row_starts = vec![0u32; right_size + 1];
    let mut columns = Vec::new();
    for (&cell, &value) in inputs.exception_cells.iter().zip(inputs.exception_values) {
        let (row, column) = (cell as usize / left_size, cell as usize % left_size);
        if shape.contains(row, column) {
            continue;
        }
        let total = computed_total(inputs, starts, row, column);
        let computed = int_cost(total, inputs.cost_factor, inputs.shift);
        match i32::from(value) - i32::from(computed) {
            1 => columns.push(column as u16 | ABOVE),
            -1 => columns.push(column as u16),
            _ => {
                return Err(Error::Unsupported(
                    "an exception differs from the computed cost by more than one",
                ));
            }
        }
        row_starts[row + 1] += 1;
    }
    for row in 0..right_size {
        row_starts[row + 1] += row_starts[row];
    }
    Ok(ExceptionColumns {
        row_starts,
        columns,
    })
}

struct RenumberedClusters {
    left_keys: Vec<u16>,
    right_keys: Vec<u16>,
    tables: Vec<u8>,
    shapes: Vec<[u32; 8]>,
}

fn reordered_cells(
    table: &[i32],
    shape: &ClusterShape,
    left: &Renumbered,
    right: &Renumbered,
) -> Vec<i32> {
    let mut cells = Vec::new();
    for (new_right, &old_right) in right.old_of_new.iter().enumerate() {
        let row = &table[usize::from(old_right) * shape.left_keys..][..shape.left_keys];
        let wanted = if new_right < right.mixed {
            shape.left_keys
        } else {
            left.mixed
        };
        cells.extend(
            left.old_of_new[..wanted]
                .iter()
                .map(|&old| row[usize::from(old)]),
        );
    }
    cells
}

fn renumber_clusters(
    inputs: &RuntimeInputs,
    starts: &[usize],
    (hot_left, hot_right): (usize, usize),
    min_sparse_gain: usize,
) -> RenumberedClusters {
    let count = inputs.shapes.len();
    let mut result = RenumberedClusters {
        left_keys: vec![0u16; inputs.left_size * count],
        right_keys: vec![0u16; inputs.right_size * count],
        tables: Vec::new(),
        shapes: Vec::new(),
    };
    for (cluster, (shape, &original_start)) in inputs.shapes.iter().zip(starts).enumerate() {
        let side = |keys: &[u16], key_count, hot| {
            renumber(
                keys.iter().skip(cluster).step_by(count).copied(),
                key_count,
                hot,
            )
        };
        let left = side(inputs.left_keys, shape.left_keys, hot_left);
        let right = side(inputs.right_keys, shape.right_keys, hot_right);
        for (new_keys, old_keys, renumbered) in [
            (&mut result.left_keys, inputs.left_keys, &left),
            (&mut result.right_keys, inputs.right_keys, &right),
        ] {
            for (new, old) in new_keys
                .iter_mut()
                .skip(cluster)
                .step_by(count)
                .zip(old_keys.iter().skip(cluster).step_by(count))
            {
                *new = renumbered.new_of_old[usize::from(*old)];
            }
        }
        let table = &inputs.tables[original_start..];
        let cells = reordered_cells(table, shape, &left, &right);
        let encoded = encode_table(&cells, min_sparse_gain);
        result.tables.extend_from_slice(&encoded.bytes);
        result.shapes.push([
            shape.left_keys as u32,
            shape.right_keys as u32,
            left.mixed as u32,
            right.mixed as u32,
            u32::from(encoded.sparse),
            encoded.width as u32,
            encoded.base as u32,
            encoded.nonzero as u32,
        ]);
    }
    result
}

fn write_runtime(
    inputs: &RuntimeInputs,
    shape: &HotShape,
    min_sparse_gain: usize,
) -> Result<Vec<u8>, Error> {
    let (left_size, right_size) = (inputs.left_size, inputs.right_size);
    if !shape.fits(left_size, right_size) {
        return Err(Error::Corrupt("the hot block lies outside of the matrix"));
    }
    let hot = shape.inscribed_rectangle();
    if left_size > usize::from(!ABOVE) {
        return Err(Error::Unsupported(
            "too many left ids for the exceptions of a runtime form",
        ));
    }
    let starts = cluster_starts(inputs);
    let exceptions = exceptions_outside_hot(inputs, shape, &starts)?;
    let clusters = renumber_clusters(inputs, &starts, hot, min_sparse_gain);
    assemble_runtime(inputs, hot, &exceptions, &clusters)
}

fn assemble_runtime(
    inputs: &RuntimeInputs,
    (hot_left, hot_right): (usize, usize),
    exceptions: &ExceptionColumns,
    clusters: &RenumberedClusters,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    for word in [
        RUNTIME_VERSION,
        inputs.cost_factor,
        inputs.shift,
        inputs.left_size as u32,
        inputs.right_size as u32,
        hot_left as u32,
        hot_right as u32,
        exceptions.columns.len() as u32,
        inputs.shapes.len() as u32,
    ] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    for word in clusters.shapes.iter().flatten() {
        out.extend_from_slice(&word.to_le_bytes());
    }
    pad(&mut out);
    let io_error = |_| Error::Corrupt("a runtime template array could not be written");
    write_array(&mut out, &exceptions.row_starts, u32::to_le_bytes).map_err(io_error)?;
    write_array(&mut out, &exceptions.columns, u16::to_le_bytes).map_err(io_error)?;
    write_array(&mut out, &clusters.left_keys, u16::to_le_bytes).map_err(io_error)?;
    write_array(&mut out, &clusters.right_keys, u16::to_le_bytes).map_err(io_error)?;
    out.extend_from_slice(&clusters.tables);
    out.extend_from_slice(&[0; TABLE_SLACK]);
    pad(&mut out);
    Ok(out)
}

struct EncodedTable {
    sparse: bool,
    width: usize,
    base: i32,
    nonzero: usize,
    bytes: Vec<u8>,
}

fn spread_of(values: impl Iterator<Item = i32> + Clone) -> (i32, usize) {
    let base = values.clone().min().unwrap_or(0);
    let range = values
        .map(|value| i64::from(value) - i64::from(base))
        .max()
        .unwrap_or(0);
    (
        base,
        [2, 3, 4][usize::from(range >= 1 << 16) + usize::from(range >= 1 << 24)],
    )
}

fn push_cell(bytes: &mut Vec<u8>, value: i32, base: i32, width: usize) {
    let stored = (i64::from(value) - i64::from(base)) as u32;
    bytes.extend_from_slice(&stored.to_le_bytes()[..width]);
}

fn encode_table(cells: &[i32], min_sparse_gain: usize) -> EncodedTable {
    let (dense_base, dense_width) = spread_of(cells.iter().copied());
    let nonzero: Vec<i32> = cells.iter().copied().filter(|&cell| cell != 0).collect();
    let (base, width) = spread_of(nonzero.iter().copied());
    let sparse_bytes = cells.len().div_ceil(BLOCK_CELLS) * BLOCK_BYTES + nonzero.len() * width;
    let mut bytes = Vec::new();
    if sparse_bytes + min_sparse_gain <= cells.len() * dense_width {
        let mut before = 0u32;
        for block in cells.chunks(BLOCK_CELLS) {
            let bits = block
                .iter()
                .enumerate()
                .fold(0u64, |bits, (at, &cell)| bits | u64::from(cell != 0) << at);
            bytes.extend_from_slice(&before.to_le_bytes());
            bytes.extend_from_slice(&bits.to_le_bytes());
            before += bits.count_ones();
        }
        for &value in &nonzero {
            push_cell(&mut bytes, value, base, width);
        }
        return EncodedTable {
            sparse: true,
            width,
            base,
            nonzero: nonzero.len(),
            bytes,
        };
    }
    for &cell in cells {
        push_cell(&mut bytes, cell, dense_base, dense_width);
    }
    EncodedTable {
        sparse: false,
        width: dense_width,
        base: dense_base,
        nonzero: 0,
        bytes,
    }
}

fn pad(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(8), 0);
}

struct Layout {
    cluster: usize,
    table_start: usize,
    left_mixed: usize,
    left_gap: usize,
    right_mixed: usize,
    stride: usize,
    mask: u32,
}

impl Layout {
    #[inline]
    fn index(&self, left: u16, right: u16) -> usize {
        let (left, right) = (usize::from(left), usize::from(right));
        right * self.left_mixed + left + right.min(self.right_mixed) * self.left_gap
    }

    #[inline]
    fn word_at(&self, start: usize, position: usize) -> usize {
        start + position * self.stride
    }
}

struct SparseShape {
    layout: Layout,
    values_start: usize,
    base: i64,
}

type Limits = (usize, usize, usize, usize);

fn keys_fit(
    keys: &[u16],
    hot: usize,
    limits: &[Limits],
    pick: fn(&Limits) -> (usize, usize),
) -> bool {
    keys.chunks_exact(limits.len().max(1))
        .enumerate()
        .all(|(id, row)| {
            row.iter().zip(limits).all(|(&key, limit)| {
                let (all, mixed) = pick(limit);
                usize::from(key) < if id < hot { all } else { mixed }
            })
        })
}

struct RuntimeHeader {
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    hot_left: usize,
    hot_right: usize,
    exception_count: usize,
    cluster_count: usize,
}

impl RuntimeHeader {
    fn read(reader: &mut TemplateReader) -> Result<Self, Error> {
        if reader.bytes::<4>()? != *MAGIC || u32::from_le_bytes(reader.bytes()?) != RUNTIME_VERSION
        {
            return Err(reader.broken("the file is not a runtime integer template file"));
        }
        let cost_factor = reader.count()? as u32;
        let shift = reader.count()? as u32;
        let (left_size, right_size) = (reader.count()?, reader.count()?);
        let (hot_left, hot_right) = (reader.count()?, reader.count()?);
        let exception_count = reader.count()?;
        let cluster_count = reader.count()?;
        if left_size == 0
            || right_size == 0
            || left_size > usize::from(!ABOVE)
            || right_size > MAX_KEYS
        {
            return Err(reader.broken("the template matrix size is out of range"));
        }
        if hot_left == 0 || hot_right == 0 || hot_left > left_size || hot_right > right_size {
            return Err(reader.broken("the hot block lies outside of the matrix"));
        }
        if shift > MAX_SHIFT || cost_factor > MAX_COST_FACTOR || cluster_count > MAX_SHAPES {
            return Err(reader.broken("the integer templates are inconsistent"));
        }
        Ok(Self {
            cost_factor,
            shift,
            left_size,
            right_size,
            hot_left,
            hot_right,
            exception_count,
            cluster_count,
        })
    }
}

struct ClusterLayouts {
    dense: Vec<Layout>,
    sparse: Vec<SparseShape>,
    limits: Vec<Limits>,
    table_bytes: usize,
    base: i64,
    sparse_tables: Vec<(usize, usize, usize)>,
}

fn table_cells(
    (left_keys, left_mixed): (usize, usize),
    (right_keys, right_mixed): (usize, usize),
) -> Option<usize> {
    (left_mixed <= left_keys && right_mixed <= right_keys)
        .then(|| {
            right_mixed
                .checked_mul(left_keys)?
                .checked_add((right_keys - right_mixed).checked_mul(left_mixed)?)
        })
        .flatten()
}

fn table_byte_count(
    cells: Option<usize>,
    sparse: usize,
    nonzero: usize,
    width: usize,
) -> Option<usize> {
    match (cells, sparse) {
        (Some(cells), 0) if nonzero == 0 => cells.checked_mul(width),
        (Some(cells), 1) if nonzero <= cells => cells
            .div_ceil(BLOCK_CELLS)
            .checked_mul(BLOCK_BYTES)
            .zip(nonzero.checked_mul(width))
            .and_then(|(blocks, values)| blocks.checked_add(values)),
        _ => None,
    }
}

impl ClusterLayouts {
    fn read(reader: &mut TemplateReader, header: &RuntimeHeader) -> Result<Self, Error> {
        let mut layouts = Self {
            dense: Vec::new(),
            sparse: Vec::new(),
            limits: Vec::with_capacity(header.cluster_count),
            table_bytes: 0,
            base: 0,
            sparse_tables: Vec::new(),
        };
        let mut magnitude = 0u128;
        for _ in 0..header.cluster_count {
            magnitude += layouts.read_cluster(reader)?;
        }
        if magnitude * u128::from(header.cost_factor) >= CELL_SUM_BOUND {
            return Err(reader.broken("the integer templates are inconsistent"));
        }
        Ok(layouts)
    }

    fn read_cluster(&mut self, reader: &mut TemplateReader) -> Result<u128, Error> {
        let left_keys = reader.count()?;
        let right_keys = reader.count()?;
        let left_mixed = reader.count()?;
        let right_mixed = reader.count()?;
        let sparse = reader.count()?;
        let width = reader.count()?;
        let cluster_base = i64::from(reader.count()? as u32 as i32);
        let nonzero = reader.count()?;
        let cells = table_cells((left_keys, left_mixed), (right_keys, right_mixed));
        let bytes = table_byte_count(cells, sparse, nonzero, width);
        let (Some(bytes), true) = (bytes, (2..=4).contains(&width)) else {
            return Err(reader.broken("a cluster table is malformed"));
        };
        let blocks_bytes = cells.map_or(0, |cells| cells.div_ceil(BLOCK_CELLS) * BLOCK_BYTES);
        if sparse == 1 {
            self.sparse_tables
                .push((self.table_bytes, cells.unwrap_or(0), nonzero));
        }
        let layout = Layout {
            cluster: self.limits.len(),
            table_start: self.table_bytes,
            left_mixed,
            left_gap: left_keys - left_mixed,
            right_mixed,
            stride: width,
            mask: u32::MAX >> (32 - 8 * width),
        };
        if sparse == 1 {
            self.sparse.push(SparseShape {
                layout,
                values_start: self.table_bytes + blocks_bytes,
                base: cluster_base,
            });
        } else {
            self.dense.push(layout);
        }
        self.limits
            .push((left_keys, left_mixed, right_keys, right_mixed));
        self.table_bytes = self
            .table_bytes
            .checked_add(bytes)
            .ok_or_else(|| reader.broken("a cluster table is malformed"))?;
        if sparse == 0 {
            self.base += cluster_base;
        }
        Ok(u128::from(cluster_base.unsigned_abs()) + (1u128 << (8 * width)))
    }
}

fn sparse_tables_fit(bytes: &[u8], sparse_tables: &[(usize, usize, usize)]) -> bool {
    sparse_tables.iter().all(|&(start, cells, nonzero)| {
        let mut before = 0u32;
        let blocks = bytes[start..]
            .as_chunks::<BLOCK_BYTES>()
            .0
            .iter()
            .take(cells.div_ceil(BLOCK_CELLS));
        let mut seen = 0;
        for (at, block) in blocks.enumerate() {
            let (count, bits) = block.split_at(4);
            let bits = u64::from_le_bytes(bits.try_into().expect("eight bytes"));
            let used = (cells - at * BLOCK_CELLS).min(BLOCK_CELLS);
            let outside = if used == BLOCK_CELLS { 0 } else { bits >> used };
            if count != before.to_le_bytes() || outside != 0 {
                return false;
            }
            before += bits.count_ones();
            seen += 1;
        }
        seen == cells.div_ceil(BLOCK_CELLS) && before as usize == nonzero
    })
}

fn exception_rows_fit(
    starts: &[u32],
    listed: &[u16],
    exception_count: usize,
    left_size: usize,
) -> bool {
    starts.first() == Some(&0)
        && starts
            .last()
            .is_some_and(|&end| end as usize == exception_count)
        && starts.windows(2).all(|span| {
            span[0] <= span[1]
                && listed[span[0] as usize..span[1] as usize]
                    .windows(2)
                    .all(|pair| pair[0] & !ABOVE < pair[1] & !ABOVE)
        })
        && listed
            .iter()
            .all(|&column| usize::from(column & !ABOVE) < left_size)
}

pub struct RuntimeIntTemplates<S = Mmap> {
    map: S,
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    hot_left: usize,
    hot_right: usize,
    cluster_count: usize,
    dense: Vec<Layout>,
    sparse: Vec<SparseShape>,
    base: i64,
    row_starts: Section<u32>,
    columns: Section<u16>,
    left_keys: Section<u16>,
    right_keys: Section<u16>,
    tables: Section<u8>,
}

impl RuntimeIntTemplates<Mmap> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::from_source(map_file(path)?, path)
    }
}

impl RuntimeIntTemplates<MappedBytes> {
    pub(crate) fn from_mapped(map: MappedBytes, path: &Path) -> Result<Self, Error> {
        Self::from_source(map, path)
    }
}

pub(crate) fn is_runtime_form(bytes: &[u8]) -> bool {
    bytes.get(..4) == Some(MAGIC.as_slice())
        && bytes.get(4..8) == Some(RUNTIME_VERSION.to_le_bytes().as_slice())
}

#[allow(private_bounds)]
impl<S: SectionSource> RuntimeIntTemplates<S> {
    fn from_source(map: S, path: &Path) -> Result<Self, Error> {
        let mut reader = TemplateReader::new(map.all_bytes(), path);
        let header = RuntimeHeader::read(&mut reader)?;
        let layouts = ClusterLayouts::read(&mut reader, &header)?;
        let row_starts: Section<u32> = reader.section(header.right_size + 1)?;
        let columns: Section<u16> = reader.section(header.exception_count)?;
        let left_keys: Section<u16> = reader.section(header.left_size * header.cluster_count)?;
        let right_keys: Section<u16> = reader.section(header.right_size * header.cluster_count)?;
        let tables: Section<u8> = reader.section(layouts.table_bytes + TABLE_SLACK)?;
        if !reader.at_end() {
            return Err(reader.broken("the file has trailing bytes"));
        }
        if !sparse_tables_fit(tables.view_source(&map), &layouts.sparse_tables) {
            return Err(reader.broken("a sparse cluster table is inconsistent"));
        }
        let rows_fit = exception_rows_fit(
            row_starts.view_source(&map),
            columns.view_source(&map),
            header.exception_count,
            header.left_size,
        );
        if !rows_fit {
            return Err(reader.broken("the exception list is inconsistent"));
        }
        let valid = keys_fit(
            left_keys.view_source(&map),
            header.hot_left,
            &layouts.limits,
            |l| (l.0, l.1),
        ) && keys_fit(
            right_keys.view_source(&map),
            header.hot_right,
            &layouts.limits,
            |l| (l.2, l.3),
        );
        if !valid {
            return Err(reader.broken("a cluster key is out of range"));
        }
        Ok(Self {
            map,
            cost_factor: header.cost_factor,
            shift: header.shift,
            left_size: header.left_size,
            right_size: header.right_size,
            hot_left: header.hot_left,
            hot_right: header.hot_right,
            cluster_count: header.cluster_count,
            dense: layouts.dense,
            sparse: layouts.sparse,
            base: layouts.base,
            row_starts,
            columns,
            left_keys,
            right_keys,
            tables,
        })
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn hot_left(&self) -> usize {
        self.hot_left
    }

    pub fn hot_right(&self) -> usize {
        self.hot_right
    }

    pub fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let (column, row) = (usize::from(left_node_rc), usize::from(right_node_lc));
        assert!(
            column >= self.hot_left || row >= self.hot_right,
            "the cell lies inside of the hot block"
        );
        assert!(
            column < self.left_size && row < self.right_size,
            "the cell lies outside of the matrix"
        );
        let file = self.map.all_bytes();
        let computed = int_cost(
            self.total(file, left_node_rc, right_node_lc),
            self.cost_factor,
            self.shift,
        );
        let starts = unsafe { self.row_starts.view_in(file) };
        debug_assert!(row + 1 < starts.len());
        let span =
            unsafe { *starts.get_unchecked(row) as usize..*starts.get_unchecked(row + 1) as usize };
        if span.is_empty() {
            return computed;
        }
        let listed = unsafe { self.columns.view_in(file).get_unchecked(span) };
        match listed.binary_search_by(|exception| (exception & !ABOVE).cmp(&left_node_rc)) {
            Ok(at) if listed[at] & ABOVE != 0 => computed + 1,
            Ok(_) => computed - 1,
            Err(_) => computed,
        }
    }

    #[inline]
    fn total(&self, file: &[u8], column: u16, row: u16) -> i64 {
        let count = self.cluster_count;
        let (column, row) = (usize::from(column), usize::from(row));
        let (lefts, rights, tables) = unsafe {
            (
                self.left_keys.view_in(file),
                self.right_keys.view_in(file),
                self.tables.view_in(file),
            )
        };
        debug_assert!((column + 1) * count <= lefts.len() && (row + 1) * count <= rights.len());
        let lefts = unsafe { lefts.as_ptr().add(column * count) };
        let rights = unsafe { rights.as_ptr().add(row * count) };
        let mut total = self.base;
        let index_of = |layout: &Layout| unsafe {
            layout.index(*lefts.add(layout.cluster), *rights.add(layout.cluster))
        };
        for layout in &self.dense {
            let at = layout.word_at(layout.table_start, index_of(layout));
            total += i64::from(unsafe { read_word(tables, at) } & layout.mask);
        }
        for shape in &self.sparse {
            let layout = &shape.layout;
            let index = index_of(layout);
            let block_at = layout.table_start + index / BLOCK_CELLS * BLOCK_BYTES;
            let (before, bits) = unsafe { read_block(tables, block_at) };
            let bit = index % BLOCK_CELLS;
            if bits >> bit & 1 != 0 {
                let rank = before as usize + (bits & ((1 << bit) - 1)).count_ones() as usize;
                let value = unsafe { read_word(tables, layout.word_at(shape.values_start, rank)) };
                total += shape.base + i64::from(value & layout.mask);
            }
        }
        total
    }
}

#[inline]
unsafe fn read_word(tables: &[u8], at: usize) -> u32 {
    debug_assert!(at + size_of::<u32>() <= tables.len());
    u32::from_le(unsafe { tables.as_ptr().add(at).cast::<u32>().read_unaligned() })
}

#[inline]
unsafe fn read_block(tables: &[u8], at: usize) -> (u32, u64) {
    debug_assert!(at + BLOCK_BYTES <= tables.len());
    let block = unsafe { tables.as_ptr().add(at) };
    (
        u32::from_le(unsafe { block.cast::<u32>().read_unaligned() }),
        u64::from_le(unsafe { block.add(size_of::<u32>()).cast::<u64>().read_unaligned() }),
    )
}
