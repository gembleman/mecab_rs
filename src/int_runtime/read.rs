use crate::error::Error;
use crate::hot_shape::{CELL_SUM_BOUND, MAX_KEYS};
use crate::int_templates::{MAGIC, MAX_COST_FACTOR, MAX_SHIFT};
use crate::section::{Section, SectionSource};
use crate::templates::TemplateReader;

use super::{
    ABOVE, BLOCK_BYTES, BLOCK_CELLS, Layout, MAX_SHAPES, RUNTIME_VERSION, SparseShape, TABLE_SLACK,
};

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

pub(super) struct RuntimeHeader {
    pub(super) cost_factor: u32,
    pub(super) shift: u32,
    pub(super) left_size: usize,
    pub(super) right_size: usize,
    pub(super) hot_left: usize,
    pub(super) hot_right: usize,
    exception_count: usize,
    pub(super) cluster_count: usize,
}

impl RuntimeHeader {
    pub(super) fn read(reader: &mut TemplateReader) -> Result<Self, Error> {
        expect_magic(reader)?;
        let cost_factor = reader.count()? as u32;
        let shift = reader.count()? as u32;
        let (left_size, right_size) = (reader.count()?, reader.count()?);
        let (hot_left, hot_right) = (reader.count()?, reader.count()?);
        let exception_count = reader.count()?;
        let cluster_count = reader.count()?;
        check_matrix_size(reader, left_size, right_size)?;
        check_hot_block(reader, (hot_left, hot_right), (left_size, right_size))?;
        check_scalars(reader, shift, cost_factor, cluster_count)?;
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

fn expect_magic(reader: &mut TemplateReader) -> Result<(), Error> {
    if reader.bytes::<4>()? != *MAGIC || u32::from_le_bytes(reader.bytes()?) != RUNTIME_VERSION {
        return Err(reader.broken("the file is not a runtime integer template file"));
    }
    Ok(())
}

fn check_matrix_size(
    reader: &TemplateReader,
    left_size: usize,
    right_size: usize,
) -> Result<(), Error> {
    let fits = left_size != 0
        && right_size != 0
        && left_size <= usize::from(!ABOVE)
        && right_size <= MAX_KEYS;
    if fits {
        Ok(())
    } else {
        Err(reader.broken("the template matrix size is out of range"))
    }
}

fn check_hot_block(
    reader: &TemplateReader,
    (hot_left, hot_right): (usize, usize),
    (left_size, right_size): (usize, usize),
) -> Result<(), Error> {
    let fits = hot_left != 0 && hot_right != 0 && hot_left <= left_size && hot_right <= right_size;
    if fits {
        Ok(())
    } else {
        Err(reader.broken("the hot block lies outside of the matrix"))
    }
}

fn check_scalars(
    reader: &TemplateReader,
    shift: u32,
    cost_factor: u32,
    cluster_count: usize,
) -> Result<(), Error> {
    if shift > MAX_SHIFT || cost_factor > MAX_COST_FACTOR || cluster_count > MAX_SHAPES {
        return Err(reader.broken("the integer templates are inconsistent"));
    }
    Ok(())
}

pub(super) struct ClusterLayouts {
    pub(super) dense: Vec<Layout>,
    pub(super) sparse: Vec<SparseShape>,
    limits: Vec<ClusterLimits>,
    table_bytes: usize,
    pub(super) base: i64,
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
    pub(super) fn read(reader: &mut TemplateReader, header: &RuntimeHeader) -> Result<Self, Error> {
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

pub(super) struct RuntimeArrays {
    pub(super) row_starts: Section<u32>,
    pub(super) columns: Section<u16>,
    pub(super) left_keys: Section<u16>,
    pub(super) right_keys: Section<u16>,
    pub(super) tables: Section<u8>,
}

impl RuntimeArrays {
    pub(super) fn read(
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

    pub(super) fn validate(
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
