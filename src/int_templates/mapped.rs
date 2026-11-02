use std::fmt;
use std::path::Path;

use super::cluster::ClusterTables;
use super::{MAX_PARTS, RuntimeInputs, TemplateHeader, int_cost, validate};
use crate::clusters::MappedClusters;
use crate::error::Error;
use crate::mapped_file::MappedFile;
use crate::part::{MappedPart, total_of};
use crate::section::{Section, SectionSource};
use crate::templates::TemplateReader;

impl<S> fmt::Debug for MappedIntTemplates<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MappedIntTemplates")
            .field("cost_factor", &self.cost_factor)
            .field("shift", &self.shift)
            .field("left_size", &self.left_size)
            .field("right_size", &self.right_size)
            .field("parts", &self.parts.len())
            .finish_non_exhaustive()
    }
}

pub struct MappedIntTemplates<S = MappedFile> {
    map: S,
    cost_factor: u32,
    shift: u32,
    left_size: usize,
    right_size: usize,
    parts: Vec<MappedPart<i32>>,
    exception_cells: Section<u32>,
    exception_values: Section<i16>,
    clusters: Option<MappedClusters<i32>>,
    exception_rows: Vec<u32>,
}

struct Body {
    parts: Vec<MappedPart<i32>>,
    exception_cells: Section<u32>,
    exception_values: Section<i16>,
    clusters: Option<MappedClusters<i32>>,
}

fn read_body(reader: &mut TemplateReader, header: &TemplateHeader) -> Result<Body, Error> {
    let (left_size, right_size) = (header.left_size, header.right_size);
    let mut parts = Vec::with_capacity(header.part_count.min(MAX_PARTS));
    for _ in 0..header.part_count {
        parts.push(MappedPart::read(reader, left_size, right_size)?);
    }
    let exception_cells = reader.section(header.exception_count)?;
    let exception_values = reader.section(header.exception_count)?;
    let clusters = match header.cluster_count {
        0 => None,
        count => Some(MappedClusters::read(reader, count, left_size, right_size)?),
    };
    if !reader.at_end() {
        return Err(reader.broken("the file has trailing bytes"));
    }
    Ok(Body {
        parts,
        exception_cells,
        exception_values,
        clusters,
    })
}

fn exception_row_starts(cells: &[u32], left_size: usize, right_size: usize) -> Vec<u32> {
    let mut exception_rows = Vec::with_capacity(right_size + 1);
    let mut next = 0usize;
    for row in 0..=right_size {
        let first_of_row = row * left_size;
        while next < cells.len() && (cells[next] as usize) < first_of_row {
            next += 1;
        }
        exception_rows.push(next as u32);
    }
    exception_rows
}

impl MappedIntTemplates<MappedFile> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::from_source(MappedFile::open(path)?, path)
    }
}

#[allow(private_bounds, reason = "the section source bound is crate internal")]
impl<S: SectionSource> MappedIntTemplates<S> {
    pub(crate) fn from_source(map: S, path: &Path) -> Result<Self, Error> {
        let mut reader = TemplateReader::new(map.all_bytes(), path);
        let header = TemplateHeader::read(&mut reader)?;
        let body = read_body(&mut reader, &header)?;
        validate(
            body.parts.iter().map(|part| part.slices(&map)),
            header.cost_factor,
            header.shift,
            header.left_size,
            header.right_size,
            body.exception_cells.view_source(&map),
        )
        .map_err(|_| reader.broken("the integer templates are inconsistent"))?;
        let exception_rows = exception_row_starts(
            body.exception_cells.view_source(&map),
            header.left_size,
            header.right_size,
        );
        Ok(Self {
            map,
            cost_factor: header.cost_factor,
            shift: header.shift,
            left_size: header.left_size,
            right_size: header.right_size,
            parts: body.parts,
            exception_cells: body.exception_cells,
            exception_values: body.exception_values,
            clusters: body.clusters,
            exception_rows,
        })
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub(crate) fn runtime_inputs(&self) -> Option<RuntimeInputs<'_>> {
        let clusters = self.clusters.as_ref()?;
        Some(RuntimeInputs {
            cost_factor: self.cost_factor,
            shift: self.shift,
            left_size: self.left_size,
            right_size: self.right_size,
            shapes: &clusters.shapes,
            left_keys: clusters.left_keys.view_source(&self.map),
            right_keys: clusters.right_keys.view_source(&self.map),
            tables: ClusterTables::Borrowed(clusters.tables.view_source(&self.map)),
            exception_cells: self.exception_cells.view_source(&self.map),
            exception_values: self.exception_values.view_source(&self.map),
        })
    }

    pub fn try_cost(&self, left_node_rc: u16, right_node_lc: u16) -> Option<i16> {
        let inside = usize::from(left_node_rc) < self.left_size
            && usize::from(right_node_lc) < self.right_size;
        inside.then(|| self.cost(left_node_rc, right_node_lc))
    }

    fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let row = usize::from(right_node_lc);
        let cell = row * self.left_size + usize::from(left_node_rc);
        let span = self.exception_rows[row] as usize..self.exception_rows[row + 1] as usize;
        let exception = if span.is_empty() {
            None
        } else {
            let cells = &self.exception_cells.view_source(&self.map)[span.clone()];
            let found = cells.binary_search(&(cell as u32)).ok();
            found.map(|at| self.exception_values.view_source(&self.map)[span.start + at])
        };
        exception.unwrap_or_else(|| {
            let total = match &self.clusters {
                Some(clusters) => clusters.sum(&self.map, left_node_rc, right_node_lc),
                _ => total_of(
                    self.parts.iter().map(|part| part.slices(&self.map)),
                    left_node_rc,
                    right_node_lc,
                ),
            };
            int_cost(total, self.cost_factor, self.shift)
        })
    }
}
