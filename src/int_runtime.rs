use std::fmt;
use std::mem::size_of;
use std::path::Path;

use crate::error::Error;
use crate::int_templates::{MAGIC, int_cost};
use crate::mapped_bytes::MappedBytes;
use crate::mapped_file::MappedFile;
use crate::section::{Section, SectionSource};
use crate::templates::TemplateReader;

use read::{ClusterLayouts, RuntimeArrays, RuntimeHeader};

mod read;
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
const LINEAR_SEARCH_MAX: usize = 8;

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

struct DensePlan {
    cluster: usize,
    table_start: usize,
    row_bytes: usize,
    gap_bytes: usize,
    right_mixed: usize,
    stride: usize,
    mask: u32,
}

impl DensePlan {
    fn from_layout(layout: &Layout) -> Self {
        Self {
            cluster: layout.cluster,
            table_start: layout.table_start,
            row_bytes: layout.left_mixed * layout.stride,
            gap_bytes: layout.left_gap * layout.stride,
            right_mixed: layout.right_mixed,
            stride: layout.stride,
            mask: layout.mask,
        }
    }

    #[inline]
    fn byte_at(&self, left: u16, right: u16) -> usize {
        let (left, right) = (usize::from(left), usize::from(right));
        self.table_start
            + right * self.row_bytes
            + left * self.stride
            + right.min(self.right_mixed) * self.gap_bytes
    }
}

struct SparseShape {
    layout: Layout,
    values_start: usize,
    base: i64,
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
    dense: Vec<DensePlan>,
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
            dense: layouts.dense.iter().map(DensePlan::from_layout).collect(),
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
        debug_assert!(
            column >= self.hot_left || row >= self.hot_right,
            "the cell lies inside of the hot block"
        );
        assert!(
            column < self.left_size && row < self.right_size,
            "the cell lies outside of the matrix"
        );
        let file = self.map.all_bytes();
        let starts = unsafe { self.row_starts.view_in(file) };
        debug_assert!(row + 1 < starts.len());
        let span =
            unsafe { *starts.get_unchecked(row) as usize..*starts.get_unchecked(row + 1) as usize };
        let computed = int_cost(
            self.total(file, left_node_rc, right_node_lc),
            self.cost_factor,
            self.shift,
        );
        if span.is_empty() {
            return computed;
        }
        let listed = unsafe { self.columns.view_in(file).get_unchecked(span) };
        computed.saturating_add(exception_step(listed, left_node_rc))
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
        for plan in &self.dense {
            let at = unsafe { plan.byte_at(*lefts.add(plan.cluster), *rights.add(plan.cluster)) };
            total += i64::from(unsafe { read_word(tables, at) } & plan.mask);
        }
        for shape in &self.sparse {
            let layout = &shape.layout;
            let index =
                unsafe { layout.index(*lefts.add(layout.cluster), *rights.add(layout.cluster)) };
            let block_at = layout.table_start + index / BLOCK_CELLS * BLOCK_BYTES;
            let (before, bits) = unsafe { read_block(tables, block_at) };
            let bit = index % BLOCK_CELLS;
            let upto = bits << (BLOCK_CELLS - 1 - bit);
            if upto >> (BLOCK_CELLS - 1) != 0 {
                let rank = before as usize + upto.count_ones() as usize - 1;
                let value = unsafe { read_word(tables, layout.word_at(shape.values_start, rank)) };
                total += shape.base + i64::from(value & layout.mask);
            }
        }
        total
    }
}

#[inline]
fn exception_step(listed: &[u16], column: u16) -> i16 {
    let found = if listed.len() <= LINEAR_SEARCH_MAX {
        listed
            .iter()
            .copied()
            .find(|exception| exception & !ABOVE >= column)
            .filter(|exception| exception & !ABOVE == column)
    } else {
        listed
            .binary_search_by(|exception| (exception & !ABOVE).cmp(&column))
            .ok()
            .map(|at| listed[at])
    };
    match found {
        Some(exception) if exception & ABOVE != 0 => 1,
        Some(_) => -1,
        None => 0,
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
