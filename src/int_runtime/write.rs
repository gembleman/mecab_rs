use std::path::Path;

use memmap2::MmapOptions;

use super::{
    ABOVE, BLOCK_BYTES, BLOCK_CELLS, MIN_SPARSE_GAIN, RUNTIME_VERSION, SHAPE_WORDS, TABLE_SLACK,
};
use crate::clusters::ClusterShape;
use crate::dict_files::MATRIX_ITP_FILE;
use crate::error::{Error, IoAction, io_error};
use crate::hot_shape::HotShape;
use crate::int_templates::{MAGIC, MappedIntTemplates, RuntimeInputs, int_cost};
use crate::section::SectionSource;
use crate::templates::write_array;

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

#[allow(private_bounds, reason = "the section source bound is crate internal")]
impl<S: SectionSource> MappedIntTemplates<S> {
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

pub fn runtime_form_of(bytes: &[u8], shape: &HotShape) -> Result<Vec<u8>, Error> {
    let path = Path::new(MATRIX_ITP_FILE);
    let mut map = MmapOptions::new()
        .len(bytes.len().max(1))
        .map_anon()
        .map_err(io_error(IoAction::Map, path))?;
    map[..bytes.len()].copy_from_slice(bytes);
    let map = map
        .make_read_only()
        .map_err(io_error(IoAction::Map, path))?;
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
    shapes: Vec<[u32; SHAPE_WORDS]>,
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
        return Err(Error::format(
            Path::new(MATRIX_ITP_FILE),
            "the hot block lies outside of the matrix",
        ));
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
    let io_error = io_error(IoAction::Write, Path::new(MATRIX_ITP_FILE));
    write_array(&mut out, &exceptions.row_starts, u32::to_le_bytes).map_err(&io_error)?;
    write_array(&mut out, &exceptions.columns, u16::to_le_bytes).map_err(&io_error)?;
    write_array(&mut out, &clusters.left_keys, u16::to_le_bytes).map_err(&io_error)?;
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
