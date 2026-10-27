use std::collections::BTreeMap;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use memmap2::Mmap;

use crate::clusters::{MappedClusters, plan_clusters, write_clusters};
use crate::dictionary::map_file;
use crate::error::Error;
use crate::mapped_bytes::MappedBytes;
use crate::section::{Plain, Section, SectionSource};

const TEMPLATE_MAGIC: &[u8; 4] = b"MXTP";
const TEMPLATE_VERSION: u32 = 1;
const ARRAY_ALIGNMENT: usize = 8;
const MAX_PARTS: usize = 256;
const DEFAULT_CLUSTER_COUNT: usize = 12;
const ROWS_PER_BLOCK: usize = 64;
const MIN_COST: f64 = -32767.0;
const MAX_COST: f64 = 32767.0;

pub struct TemplatePart {
    pub left_count: usize,
    pub right_count: usize,
    pub left_projection: Vec<u16>,
    pub right_projection: Vec<u16>,
    pub indptr: Vec<u32>,
    pub left_index: Vec<u16>,
    pub weight: Vec<f64>,
}

pub struct ConnectionTemplates {
    cost_factor: f64,
    left_size: usize,
    right_size: usize,
    parts: Vec<TemplatePart>,
}

fn is_strictly_sorted(part: &TemplatePart) -> bool {
    part.indptr.windows(2).all(|span| {
        part.left_index[span[0] as usize..span[1] as usize]
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    })
}

fn sort_spans(part: &mut TemplatePart) {
    let mut left_index = Vec::with_capacity(part.left_index.len());
    let mut weight = Vec::with_capacity(part.weight.len());
    let mut indptr = Vec::with_capacity(part.indptr.len());
    indptr.push(0);
    for span in part.indptr.windows(2) {
        let range = span[0] as usize..span[1] as usize;
        let mut cells: Vec<(u16, f64)> = part.left_index[range.clone()]
            .iter()
            .copied()
            .zip(part.weight[range].iter().copied())
            .collect();
        cells.reverse();
        cells.sort_by_key(|&(index, _)| index);
        cells.dedup_by_key(|&mut (index, _)| index);
        for (index, value) in cells {
            left_index.push(index);
            weight.push(value);
        }
        indptr.push(left_index.len() as u32);
    }
    part.left_index = left_index;
    part.weight = weight;
    part.indptr = indptr;
}

fn validate(part: &TemplatePart, left_size: usize, right_size: usize) -> Result<(), Error> {
    let slices = PartSlices {
        left_count: part.left_count,
        right_count: part.right_count,
        left_projection: &part.left_projection,
        right_projection: &part.right_projection,
        indptr: &part.indptr,
        left_index: &part.left_index,
        weight: &part.weight,
    };
    let consistent = part.left_projection.len() == left_size
        && part.right_projection.len() == right_size
        && part.weight.len() == part.left_index.len()
        && part.indptr.len() == part.right_count + 1
        && slices.is_consistent();
    if consistent {
        Ok(())
    } else {
        Err(Error::Corrupt("a connection template is inconsistent"))
    }
}

pub(crate) fn to_cost(sum: f64, cost_factor: f64) -> i16 {
    (-cost_factor * sum).clamp(MIN_COST, MAX_COST) as i16
}

pub(crate) struct PartSlices<'a> {
    left_count: usize,
    right_count: usize,
    pub(crate) left_projection: &'a [u16],
    pub(crate) right_projection: &'a [u16],
    indptr: &'a [u32],
    left_index: &'a [u16],
    pub(crate) weight: &'a [f64],
}

impl PartSlices<'_> {
    pub(crate) fn weight_of(&self, left_node_rc: u16, right_node_lc: u16) -> Option<f64> {
        let right_key = usize::from(self.right_projection[usize::from(right_node_lc)]);
        if right_key == self.right_count {
            return None;
        }
        let left_key = self.left_projection[usize::from(left_node_rc)];
        let span = self.indptr[right_key] as usize..self.indptr[right_key + 1] as usize;
        let found = self.left_index[span.clone()]
            .binary_search(&left_key)
            .ok()?;
        Some(self.weight[span.start + found])
    }
}

fn connection_cost<'a>(
    parts: impl Iterator<Item = PartSlices<'a>>,
    cost_factor: f64,
    left_node_rc: u16,
    right_node_lc: u16,
) -> i16 {
    let weights = parts.filter_map(|part| part.weight_of(left_node_rc, right_node_lc));
    to_cost(weights.fold(0.0, |sum, weight| sum + weight), cost_factor)
}

fn padding_after(bytes: usize) -> usize {
    bytes.next_multiple_of(ARRAY_ALIGNMENT) - bytes
}

pub(crate) fn write_array<T: Copy, const WIDTH: usize>(
    output: &mut impl Write,
    items: &[T],
    encode: fn(T) -> [u8; WIDTH],
) -> io::Result<()> {
    for item in items {
        output.write_all(&encode(*item))?;
    }
    output.write_all(&[0; ARRAY_ALIGNMENT][..padding_after(items.len() * WIDTH)])
}

struct MappedPart {
    left_count: usize,
    right_count: usize,
    left_projection: Section<u16>,
    right_projection: Section<u16>,
    indptr: Section<u32>,
    left_index: Section<u16>,
    weight: Section<f64>,
}

impl MappedPart {
    fn slices<'a, S: SectionSource>(&self, file: &'a S) -> PartSlices<'a> {
        PartSlices {
            left_count: self.left_count,
            right_count: self.right_count,
            left_projection: self.left_projection.view_source(file),
            right_projection: self.right_projection.view_source(file),
            indptr: self.indptr.view_source(file),
            left_index: self.left_index.view_source(file),
            weight: self.weight.view_source(file),
        }
    }
}

pub(crate) struct TemplateReader<'a> {
    pub(crate) file: &'a [u8],
    path: &'a Path,
    offset: usize,
}

impl<'a> TemplateReader<'a> {
    pub(crate) fn new(file: &'a [u8], path: &'a Path) -> Self {
        Self {
            file,
            path,
            offset: 0,
        }
    }

    pub(crate) fn at_end(&self) -> bool {
        self.offset == self.file.len()
    }

    pub(crate) fn broken(&self, reason: &'static str) -> Error {
        Error::Format {
            file: self.path.to_path_buf(),
            reason,
        }
    }

    pub(crate) fn bytes<const WIDTH: usize>(&mut self) -> Result<[u8; WIDTH], Error> {
        let end = self.offset + WIDTH;
        let bytes = self
            .file
            .get(self.offset..end)
            .ok_or_else(|| self.broken("the file ends inside a header"))?;
        self.offset = end;
        Ok(bytes.try_into().expect("the slice has the requested width"))
    }

    pub(crate) fn count(&mut self) -> Result<usize, Error> {
        self.bytes().map(|bytes| u32::from_le_bytes(bytes) as usize)
    }

    pub(crate) fn section<T: Plain>(&mut self, count: usize) -> Result<Section<T>, Error> {
        let length = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| self.broken("an array is too long"))?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| self.broken("an array is too long"))?;
        let section = Section::new(self.file, self.offset..end)
            .ok_or_else(|| self.broken("an array is misplaced"))?;
        self.offset = end + padding_after(length);
        Ok(section)
    }
}

pub struct MappedTemplates<S = Mmap> {
    map: S,
    cost_factor: f64,
    left_size: usize,
    right_size: usize,
    parts: Vec<MappedPart>,
    clusters: Option<MappedClusters>,
}

impl MappedTemplates<Mmap> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::from_source(map_file(path)?, path)
    }
}

impl MappedTemplates<MappedBytes> {
    pub(crate) fn from_mapped(map: MappedBytes, path: &Path) -> Result<Self, Error> {
        Self::from_source(map, path)
    }
}

#[allow(private_bounds)]
impl<S: SectionSource> MappedTemplates<S> {
    fn from_source(map: S, path: &Path) -> Result<Self, Error> {
        let file = map.all_bytes();
        let mut reader = TemplateReader {
            file,
            path,
            offset: 0,
        };
        if reader.bytes::<4>()? != *TEMPLATE_MAGIC
            || u32::from_le_bytes(reader.bytes()?) != TEMPLATE_VERSION
        {
            return Err(reader.broken("the file is not a template file of this version"));
        }
        let cost_factor = f64::from_le_bytes(reader.bytes()?);
        let left_size = reader.count()?;
        let right_size = reader.count()?;
        let part_count = reader.count()?;
        let cluster_count = reader.count()?;
        if left_size == 0 || right_size == 0 || left_size > 0xFFFF || right_size > 0xFFFF {
            return Err(reader.broken("the template matrix size is out of range"));
        }
        let mut parts = Vec::with_capacity(part_count.min(MAX_PARTS));
        for _ in 0..part_count {
            let left_count = reader.count()?;
            let right_count = reader.count()?;
            let nnz = reader.count()?;
            reader.count()?;
            let part = MappedPart {
                left_count,
                right_count,
                left_projection: reader.section(left_size)?,
                right_projection: reader.section(right_size)?,
                indptr: reader.section(right_count + 1)?,
                left_index: reader.section(nnz)?,
                weight: reader.section(nnz)?,
            };
            if !part.slices(&map).is_consistent() {
                return Err(reader.broken("a connection template is inconsistent"));
            }
            parts.push(part);
        }
        let clusters = match cluster_count {
            0 => None,
            count => Some(MappedClusters::read(
                &mut reader,
                count,
                left_size,
                right_size,
            )?),
        };
        if reader.offset != file.len() {
            return Err(reader.broken("the file has trailing bytes"));
        }
        Ok(Self {
            map,
            cost_factor,
            left_size,
            right_size,
            parts,
            clusters,
        })
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let clustered = self.clusters.as_ref().and_then(|clusters| {
            clusters.cost(&self.map, self.cost_factor, left_node_rc, right_node_lc)
        });
        clustered.unwrap_or_else(|| self.exact_cost(left_node_rc, right_node_lc))
    }

    fn exact_cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let parts = self.parts.iter().map(|part| part.slices(&self.map));
        connection_cost(parts, self.cost_factor, left_node_rc, right_node_lc)
    }
}

impl PartSlices<'_> {
    fn is_consistent(&self) -> bool {
        let nnz = self.left_index.len();
        self.indptr.first() == Some(&0)
            && self.indptr.last().is_some_and(|&end| end as usize == nnz)
            && self.indptr.windows(2).all(|span| span[0] <= span[1])
            && self
                .left_projection
                .iter()
                .all(|&key| usize::from(key) <= self.left_count)
            && self
                .right_projection
                .iter()
                .all(|&key| usize::from(key) <= self.right_count)
            && self
                .left_index
                .iter()
                .all(|&key| usize::from(key) < self.left_count)
    }
}

impl ConnectionTemplates {
    pub fn new(
        cost_factor: f64,
        left_size: usize,
        right_size: usize,
        mut parts: Vec<TemplatePart>,
    ) -> Result<Self, Error> {
        if left_size == 0 || right_size == 0 || left_size > 0xFFFF || right_size > 0xFFFF {
            return Err(Error::Corrupt("the template matrix size is out of range"));
        }
        for part in &mut parts {
            validate(part, left_size, right_size)?;
            if !is_strictly_sorted(part) {
                sort_spans(part);
            }
        }
        Ok(Self {
            cost_factor,
            left_size,
            right_size,
            parts,
        })
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn cost_factor(&self) -> f64 {
        self.cost_factor
    }

    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    fn slices(&self) -> impl Iterator<Item = PartSlices<'_>> {
        self.parts.iter().map(|part| PartSlices {
            left_count: part.left_count,
            right_count: part.right_count,
            left_projection: &part.left_projection,
            right_projection: &part.right_projection,
            indptr: &part.indptr,
            left_index: &part.left_index,
            weight: &part.weight,
        })
    }

    pub fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        connection_cost(self.slices(), self.cost_factor, left_node_rc, right_node_lc)
    }

    fn fill_row_sums(&self, right_left_id: usize, hot_left: usize, scratch: &mut RowScratch) {
        scratch.row[..hot_left].fill(0.0);
        for (part, lookup) in self.slices().zip(&mut scratch.lookups) {
            let right_key = usize::from(part.right_projection[right_left_id]);
            if right_key == part.right_count {
                continue;
            }
            let span = part.indptr[right_key] as usize..part.indptr[right_key + 1] as usize;
            let left_index = &part.left_index[span.clone()];
            for (index, weight) in left_index.iter().zip(&part.weight[span]) {
                lookup[usize::from(*index)] = *weight;
            }
            for (sum, projected) in scratch.row[..hot_left].iter_mut().zip(part.left_projection) {
                *sum += lookup[usize::from(*projected)];
            }
            for index in left_index {
                lookup[usize::from(*index)] = 0.0;
            }
        }
    }

    pub fn write_templates(&self, output: &mut impl Write) -> io::Result<()> {
        self.write_clustered_templates(DEFAULT_CLUSTER_COUNT, output)
    }

    pub fn write_clustered_templates(
        &self,
        cluster_count: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let slices: Vec<PartSlices> = self.slices().collect();
        let plans = if cluster_count == 0 {
            Vec::new()
        } else {
            plan_clusters(&slices, cluster_count)
        };
        output.write_all(TEMPLATE_MAGIC)?;
        output.write_all(&TEMPLATE_VERSION.to_le_bytes())?;
        output.write_all(&self.cost_factor.to_le_bytes())?;
        for count in [
            self.left_size,
            self.right_size,
            self.parts.len(),
            plans.len(),
        ] {
            output.write_all(&(count as u32).to_le_bytes())?;
        }
        for part in &self.parts {
            for count in [part.left_count, part.right_count, part.weight.len(), 0] {
                output.write_all(&(count as u32).to_le_bytes())?;
            }
            write_array(output, &part.left_projection, u16::to_le_bytes)?;
            write_array(output, &part.right_projection, u16::to_le_bytes)?;
            write_array(output, &part.indptr, u32::to_le_bytes)?;
            write_array(output, &part.left_index, u16::to_le_bytes)?;
            write_array(output, &part.weight, f64::to_le_bytes)?;
        }
        if plans.is_empty() {
            return Ok(());
        }
        write_clusters(&slices, &plans, self.left_size, self.right_size, output)
    }

    fn render_block(
        &self,
        block: usize,
        hot_left: usize,
        hot_right: usize,
        scratch: &mut RowScratch,
    ) -> Vec<u8> {
        let first = block * ROWS_PER_BLOCK;
        let last = (first + ROWS_PER_BLOCK).min(hot_right);
        let mut bytes = Vec::with_capacity((last - first) * hot_left * 2);
        for right_left_id in first..last {
            self.fill_row_sums(right_left_id, hot_left, scratch);
            for sum in &scratch.row[..hot_left] {
                bytes.extend_from_slice(&to_cost(*sum, self.cost_factor).to_le_bytes());
            }
        }
        bytes
    }

    pub fn write_hot_matrix(
        &self,
        hot_left: usize,
        hot_right: usize,
        threads: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if hot_left > self.left_size || hot_right > self.right_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the hot block exceeds the matrix",
            ));
        }
        for size in [self.left_size, self.right_size, hot_left, hot_right] {
            output.write_all(&(size as u16).to_le_bytes())?;
        }
        let blocks = hot_right.div_ceil(ROWS_PER_BLOCK);
        let next_block = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::sync_channel::<(usize, Vec<u8>)>(threads.max(1) * 2);
        thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                let sender = sender.clone();
                let next_block = &next_block;
                scope.spawn(move || {
                    let mut scratch = RowScratch::new(self);
                    loop {
                        let block = next_block.fetch_add(1, Ordering::Relaxed);
                        if block >= blocks {
                            break;
                        }
                        let bytes = self.render_block(block, hot_left, hot_right, &mut scratch);
                        if sender.send((block, bytes)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(sender);
            let mut pending = BTreeMap::new();
            let mut next_to_write = 0;
            for (block, bytes) in receiver.iter() {
                pending.insert(block, bytes);
                while let Some(bytes) = pending.remove(&next_to_write) {
                    output.write_all(&bytes)?;
                    next_to_write += 1;
                }
            }
            if next_to_write == blocks {
                Ok(())
            } else {
                Err(io::Error::other("a template worker stopped early"))
            }
        })
    }
}

pub(crate) struct ExactRows<'a> {
    templates: &'a ConnectionTemplates,
    scratch: RowScratch,
}

impl<'a> ExactRows<'a> {
    pub(crate) fn new(templates: &'a ConnectionTemplates) -> Self {
        Self {
            templates,
            scratch: RowScratch::new(templates),
        }
    }

    pub(crate) fn render(&mut self, right_left_id: usize, out: &mut [i16]) {
        let left_size = self.templates.left_size;
        self.templates
            .fill_row_sums(right_left_id, left_size, &mut self.scratch);
        for (cost, sum) in out.iter_mut().zip(&self.scratch.row) {
            *cost = to_cost(*sum, self.templates.cost_factor);
        }
    }
}

struct RowScratch {
    row: Vec<f64>,
    lookups: Vec<Vec<f64>>,
}

impl RowScratch {
    fn new(templates: &ConnectionTemplates) -> Self {
        Self {
            row: vec![0.0; templates.left_size],
            lookups: templates
                .parts
                .iter()
                .map(|part| vec![0.0; part.left_count + 1])
                .collect(),
        }
    }
}
