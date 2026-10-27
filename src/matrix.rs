use std::path::Path;

use crate::error::Error;
use crate::int_templates::MappedIntTemplates;
use crate::mapped_bytes::MappedBytes;
use crate::section::{Section, SectionSource};
use crate::templates::MappedTemplates;

const HEADER_BYTES: usize = 4;
const HOT_HEADER_BYTES: usize = 8;
const COST_BYTES: usize = 2;

fn broken(path: &Path, reason: &'static str) -> Error {
    Error::Format {
        file: path.to_path_buf(),
        reason,
    }
}

fn header_sizes(map: &[u8], path: &Path, words: usize) -> Result<[usize; 4], Error> {
    let mut sizes = [0; 4];
    let header = map
        .get(..words * 2)
        .ok_or_else(|| broken(path, "the file is shorter than its header"))?;
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
        let [left_size, right_size, _, _] = header_sizes(map.all_bytes(), path, HEADER_BYTES / 2)?;
        if left_size == 0 || right_size == 0 {
            return Err(broken(path, "the matrix has no context ids"));
        }
        if HEADER_BYTES + left_size * right_size * COST_BYTES != map.all_bytes().len() {
            return Err(broken(path, "the file size does not match the header"));
        }
        let costs = Section::new(map.all_bytes(), HEADER_BYTES..map.all_bytes().len())
            .ok_or_else(|| broken(path, "the cost table is misaligned"))?;
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

pub(crate) enum ColdPath<S = MappedBytes> {
    Float(MappedTemplates<S>),
    Int(Box<MappedIntTemplates<S>>),
}

impl<S: SectionSource> ColdPath<S> {
    fn left_size(&self) -> usize {
        match self {
            Self::Float(templates) => templates.left_size(),
            Self::Int(templates) => templates.left_size(),
        }
    }

    fn right_size(&self) -> usize {
        match self {
            Self::Float(templates) => templates.right_size(),
            Self::Int(templates) => templates.right_size(),
        }
    }
}

pub(crate) struct HotMatrix<S = MappedBytes> {
    map: S,
    costs: Section<i16>,
    templates: ColdPath<S>,
    hot_left: usize,
    hot_right: usize,
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
    match templates {
        ColdPath::Float(templates) => templates.cost(left_right_id, right_left_id),
        ColdPath::Int(templates) => templates.cost(left_right_id, right_left_id),
    }
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
        let [left_size, right_size, hot_left, hot_right] =
            header_sizes(map.all_bytes(), path, HOT_HEADER_BYTES / 2)?;
        if left_size != templates.left_size() || right_size != templates.right_size() {
            return Err(broken(
                path,
                "the hot matrix size does not match the templates",
            ));
        }
        if hot_left == 0 || hot_right == 0 || hot_left > left_size || hot_right > right_size {
            return Err(broken(path, "the hot block lies outside of the matrix"));
        }
        if HOT_HEADER_BYTES + hot_left * hot_right * COST_BYTES != map.all_bytes().len() {
            return Err(broken(path, "the file size does not match the header"));
        }
        let costs = Section::new(map.all_bytes(), HOT_HEADER_BYTES..map.all_bytes().len())
            .ok_or_else(|| broken(path, "the cost table is misaligned"))?;
        Ok(Self {
            map,
            costs,
            templates,
            hot_left,
            hot_right,
        })
    }

    pub(crate) fn hot_left(&self) -> usize {
        self.hot_left
    }

    #[inline]
    pub(crate) fn view(&self) -> HotView<'_, S> {
        HotView {
            costs: self.costs.view_source(&self.map),
            templates: &self.templates,
            hot_left: self.hot_left,
            hot_right: self.hot_right,
        }
    }
}

pub(crate) struct HotView<'m, S = MappedBytes> {
    costs: &'m [i16],
    templates: &'m ColdPath<S>,
    hot_left: usize,
    hot_right: usize,
}

impl<'m, S: SectionSource> HotView<'m, S> {
    #[inline]
    pub(crate) fn right_column(&self, right_left_id: u16) -> HotColumn<'m, S> {
        let row = usize::from(right_left_id);
        let cells = if row < self.hot_right {
            &self.costs[row * self.hot_left..(row + 1) * self.hot_left]
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
        integer: bool,
        path: &Path,
    ) -> Result<Self, Error> {
        let templates = if integer {
            ColdPath::Int(Box::new(MappedIntTemplates::from_mapped(templates, path)?))
        } else {
            ColdPath::Float(MappedTemplates::from_mapped(templates, path)?)
        };
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
        Self::from_mapped_hot(map, templates, false, path)
    }

    pub(crate) fn open_hot_int_bytes(
        path: &Path,
        map: MappedBytes,
        templates: MappedBytes,
    ) -> Result<Self, Error> {
        Self::from_mapped_hot(map, templates, true, path)
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
