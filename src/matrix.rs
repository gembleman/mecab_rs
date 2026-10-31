use std::path::Path;

use crate::error::Error;
use crate::hot_shape::{HOT_HEADER_BYTES, HotShape};
use crate::int_runtime::{RuntimeIntTemplates, is_runtime_form};
use crate::mapped_bytes::MappedBytes;
use crate::section::{Section, SectionSource};

pub const MATRIX_HEADER_BYTES: usize = 4;
pub const MATRIX_COST_BYTES: usize = 2;

fn header_sizes(map: &[u8], path: &Path, words: usize) -> Result<[usize; 4], Error> {
    let mut sizes = [0; 4];
    let header = map
        .get(..words * 2)
        .ok_or_else(|| Error::format(path, "the file is shorter than its header"))?;
    for (size, &word) in sizes.iter_mut().zip(header.as_chunks::<2>().0) {
        *size = usize::from(u16::from_le_bytes(word));
    }
    Ok(sizes)
}

pub(crate) struct DenseMatrix<S = MappedBytes> {
    map: S,
    costs: Section<i16>,
    left_size: usize,
    right_size: usize,
}

impl<S: SectionSource> DenseMatrix<S> {
    fn from_source(map: S, path: &Path) -> Result<Self, Error> {
        let [left_size, right_size, _, _] =
            header_sizes(map.all_bytes(), path, MATRIX_HEADER_BYTES / 2)?;
        if left_size == 0 || right_size == 0 {
            return Err(Error::format(path, "the matrix has no context ids"));
        }
        if MATRIX_HEADER_BYTES + left_size * right_size * MATRIX_COST_BYTES != map.all_bytes().len()
        {
            return Err(Error::format(
                path,
                "the file size does not match the header",
            ));
        }
        let costs = Section::new(map.all_bytes(), MATRIX_HEADER_BYTES..map.all_bytes().len())
            .ok_or_else(|| Error::format(path, "the cost table is misaligned"))?;
        Ok(Self {
            map,
            costs,
            left_size,
            right_size,
        })
    }

    pub(crate) fn right_column(&self, right_left_id: u16) -> &[i16] {
        let start = self.left_size * usize::from(right_left_id);
        &self.costs.view_source(&self.map)[start..start + self.left_size]
    }
}

pub(crate) type ColdPath<S = MappedBytes> = Box<RuntimeIntTemplates<S>>;

pub(crate) struct HotMatrix<S = MappedBytes> {
    map: S,
    row_starts: Section<u32>,
    costs: Section<i16>,
    templates: ColdPath<S>,
}

pub(crate) struct HotColumn<'m, S = MappedBytes> {
    cells: &'m [i16],
    templates: &'m ColdPath<S>,
    right_left_id: u16,
}

#[cold]
#[inline(never)]
fn cold_cost<S: SectionSource>(
    templates: &ColdPath<S>,
    left_right_id: u16,
    right_left_id: u16,
) -> i16 {
    templates.cost(left_right_id, right_left_id)
}

impl<'m, S: SectionSource> HotColumn<'m, S> {
    pub(crate) fn into_cost_fn(self) -> impl FnMut(u16) -> i16 + 'm {
        let Self {
            cells,
            templates,
            right_left_id,
        } = self;
        move |left_right_id| match cells.get(usize::from(left_right_id)) {
            Some(&cost) => cost,
            None => cold_cost(templates, left_right_id, right_left_id),
        }
    }

    pub(crate) fn cells(&self) -> &'m [i16] {
        self.cells
    }
}

impl<S: SectionSource> HotMatrix<S> {
    fn from_source(map: S, templates: ColdPath<S>, path: &Path) -> Result<Self, Error> {
        let (left_size, right_size, shape) =
            HotShape::parse(map.all_bytes()).map_err(|reason| Error::format(path, reason))?;
        if left_size != templates.left_size() || right_size != templates.right_size() {
            return Err(Error::format(
                path,
                "the hot matrix size does not match the templates",
            ));
        }
        if !shape.contains_rectangle(templates.hot_left(), templates.hot_right()) {
            return Err(Error::format(
                path,
                "the integer templates were made for another hot block",
            ));
        }
        let table_end = HotShape::header_bytes(shape.rows());
        let row_starts = Section::new(map.all_bytes(), HOT_HEADER_BYTES..table_end)
            .ok_or_else(|| Error::format(path, "the row starts are misaligned"))?;
        let costs = Section::new(map.all_bytes(), table_end..map.all_bytes().len())
            .ok_or_else(|| Error::format(path, "the cost table is misaligned"))?;
        Ok(Self {
            map,
            row_starts,
            costs,
            templates,
        })
    }

    #[inline]
    pub(crate) fn view(&self) -> HotView<'_, S> {
        let file = self.map.all_bytes();
        HotView {
            row_starts: unsafe { self.row_starts.view_in(file) },
            costs: unsafe { self.costs.view_in(file) },
            templates: &self.templates,
        }
    }
}

pub(crate) struct HotView<'m, S = MappedBytes> {
    row_starts: &'m [u32],
    costs: &'m [i16],
    templates: &'m ColdPath<S>,
}

impl<'m, S: SectionSource> HotView<'m, S> {
    #[inline]
    pub(crate) fn right_column(&self, right_left_id: u16) -> HotColumn<'m, S> {
        let row = usize::from(right_left_id);
        let cells = if row + 1 < self.row_starts.len() {
            let (start, end) = unsafe {
                (
                    *self.row_starts.get_unchecked(row) as usize,
                    *self.row_starts.get_unchecked(row + 1) as usize,
                )
            };
            debug_assert!(start <= end && end <= self.costs.len());
            unsafe { self.costs.get_unchecked(start..end) }
        } else {
            &[]
        };
        HotColumn {
            cells,
            templates: self.templates,
            right_left_id,
        }
    }
}

pub(crate) enum Matrix {
    PackedDense(DenseMatrix<MappedBytes>),
    PackedHot(HotMatrix<MappedBytes>),
}

impl Matrix {
    pub(crate) fn from_mapped_dense(map: MappedBytes, path: &Path) -> Result<Self, Error> {
        DenseMatrix::from_source(map, path).map(Self::PackedDense)
    }

    pub(crate) fn from_mapped_hot(
        map: MappedBytes,
        templates: MappedBytes,
        path: &Path,
    ) -> Result<Self, Error> {
        if !is_runtime_form(&templates) {
            return Err(Error::format(
                path,
                "the integer templates are not in the runtime form",
            ));
        }
        let templates = Box::new(RuntimeIntTemplates::from_mapped(templates, path)?);
        HotMatrix::from_source(map, templates, path).map(Self::PackedHot)
    }

    pub(crate) fn open_dense_bytes(path: &Path, map: MappedBytes) -> Result<Self, Error> {
        Self::from_mapped_dense(map, path)
    }

    pub(crate) fn open_hot_bytes(
        path: &Path,
        map: MappedBytes,
        templates: MappedBytes,
    ) -> Result<Self, Error> {
        Self::from_mapped_hot(map, templates, path)
    }

    pub(crate) fn left_size(&self) -> usize {
        match self {
            Self::PackedDense(dense) => dense.left_size,
            Self::PackedHot(hot) => hot.templates.left_size(),
        }
    }

    pub(crate) fn right_size(&self) -> usize {
        match self {
            Self::PackedDense(dense) => dense.right_size,
            Self::PackedHot(hot) => hot.templates.right_size(),
        }
    }
}
