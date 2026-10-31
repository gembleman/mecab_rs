use std::fmt;
use std::mem::size_of;
use std::path::Path;

use crate::error::Error;
use crate::hot_shape::{CELL_SUM_BOUND, MAX_KEYS};
use crate::int_templates::{MAGIC, MAX_COST_FACTOR, MAX_SHIFT, int_cost};
use crate::mapped_bytes::MappedBytes;
use crate::mapped_file::MappedFile;
use crate::section::{Section, SectionSource};
use crate::templates::TemplateReader;

mod write;

#[cfg(feature = "xz")]
pub(crate) use write::runtime_form_from;
pub use write::runtime_form_of;

pub(crate) const RUNTIME_VERSION: u32 = 2;
const MAX_SHAPES: usize = 256;
const TABLE_SLACK: usize = 4;
const BLOCK_CELLS: usize = 64;
const BLOCK_BYTES: usize = 12;
const MIN_SPARSE_GAIN: usize = 1 << 18;
const ABOVE: u16 = 1 << 15;
const SHAPE_WORDS: usize = 8;

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

struct ClusterLimits {
    left_keys: usize,
    left_mixed: usize,
    right_keys: usize,
    right_mixed: usize,
}

impl ClusterLimits {
    fn left(&self) -> (usize, usize) {
        (self.left_keys, self.left_mixed)
    }

    fn right(&self) -> (usize, usize) {
        (self.right_keys, self.right_mixed)
    }
}

fn keys_fit(
    keys: &[u16],
    hot: usize,
    limits: &[ClusterLimits],
    pick: fn(&ClusterLimits) -> (usize, usize),
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
    limits: Vec<ClusterLimits>,
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
        self.limits.push(ClusterLimits {
            left_keys,
            left_mixed,
            right_keys,
            right_mixed,
        });
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
            listed
                .get(span[0] as usize..span[1] as usize)
                .is_some_and(|row| {
                    row.windows(2)
                        .all(|pair| pair[0] & !ABOVE < pair[1] & !ABOVE)
                })
        })
        && listed
            .iter()
            .all(|&column| usize::from(column & !ABOVE) < left_size)
}

struct RuntimeArrays {
    row_starts: Section<u32>,
    columns: Section<u16>,
    left_keys: Section<u16>,
    right_keys: Section<u16>,
    tables: Section<u8>,
}

impl RuntimeArrays {
    fn read(
        reader: &mut TemplateReader,
        header: &RuntimeHeader,
        layouts: &ClusterLayouts,
    ) -> Result<Self, Error> {
        let arrays = Self {
            row_starts: reader.section(header.right_size + 1)?,
            columns: reader.section(header.exception_count)?,
            left_keys: reader.section(header.left_size * header.cluster_count)?,
            right_keys: reader.section(header.right_size * header.cluster_count)?,
            tables: reader.section(layouts.table_bytes + TABLE_SLACK)?,
        };
        if !reader.at_end() {
            return Err(reader.broken("the file has trailing bytes"));
        }
        Ok(arrays)
    }

    fn validate(
        &self,
        map: &impl SectionSource,
        reader: &TemplateReader,
        header: &RuntimeHeader,
        layouts: &ClusterLayouts,
    ) -> Result<(), Error> {
        if !sparse_tables_fit(self.tables.view_source(map), &layouts.sparse_tables) {
            return Err(reader.broken("a sparse cluster table is inconsistent"));
        }
        let rows_fit = exception_rows_fit(
            self.row_starts.view_source(map),
            self.columns.view_source(map),
            header.exception_count,
            header.left_size,
        );
        if !rows_fit {
            return Err(reader.broken("the exception list is inconsistent"));
        }
        let valid = keys_fit(
            self.left_keys.view_source(map),
            header.hot_left,
            &layouts.limits,
            ClusterLimits::left,
        ) && keys_fit(
            self.right_keys.view_source(map),
            header.hot_right,
            &layouts.limits,
            ClusterLimits::right,
        );
        if !valid {
            return Err(reader.broken("a cluster key is out of range"));
        }
        Ok(())
    }
}

impl<S> fmt::Debug for RuntimeIntTemplates<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeIntTemplates")
            .field("cost_factor", &self.cost_factor)
            .field("shift", &self.shift)
            .field("left_size", &self.left_size)
            .field("right_size", &self.right_size)
            .field("cluster_count", &self.cluster_count)
            .finish_non_exhaustive()
    }
}

pub struct RuntimeIntTemplates<S = MappedFile> {
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

impl RuntimeIntTemplates<MappedFile> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::from_source(MappedFile::open(path)?, path)
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

#[allow(private_bounds, reason = "the section source bound is crate internal")]
impl<S: SectionSource> RuntimeIntTemplates<S> {
    fn from_source(map: S, path: &Path) -> Result<Self, Error> {
        let mut reader = TemplateReader::new(map.all_bytes(), path);
        let header = RuntimeHeader::read(&mut reader)?;
        let layouts = ClusterLayouts::read(&mut reader, &header)?;
        let arrays = RuntimeArrays::read(&mut reader, &header, &layouts)?;
        arrays.validate(&map, &reader, &header, &layouts)?;
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
            row_starts: arrays.row_starts,
            columns: arrays.columns,
            left_keys: arrays.left_keys,
            right_keys: arrays.right_keys,
            tables: arrays.tables,
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

    pub fn try_cost(&self, left_node_rc: u16, right_node_lc: u16) -> Option<i16> {
        let (column, row) = (usize::from(left_node_rc), usize::from(right_node_lc));
        let in_cold_part = column >= self.hot_left || row >= self.hot_right;
        let inside = column < self.left_size && row < self.right_size;
        (in_cold_part && inside).then(|| self.cost(left_node_rc, right_node_lc))
    }

    pub(crate) fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
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
            Ok(at) if listed[at] & ABOVE != 0 => computed.saturating_add(1),
            Ok(_) => computed.saturating_sub(1),
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
