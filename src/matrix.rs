use std::path::Path;

use memmap2::Mmap;

use crate::dictionary::map_file;
use crate::error::Error;
use crate::section::Section;
use crate::templates::MappedTemplates;

const HEADER_BYTES: usize = 4;
const HOT_HEADER_BYTES: usize = 8;
const COST_BYTES: usize = 2;

fn broken(path: &Path, reason: &'static str) -> Error {
    Error::Format { file: path.to_path_buf(), reason }
}

fn header_sizes(map: &Mmap, path: &Path, words: usize) -> Result<[usize; 4], Error> {
    let mut sizes = [0; 4];
    let header = map.get(..words * 2).ok_or_else(|| broken(path, "the file is shorter than its header"))?;
    for (size, &word) in sizes.iter_mut().zip(header.as_chunks::<2>().0) {
        *size = usize::from(u16::from_le_bytes(word));
    }
    Ok(sizes)
}

pub(crate) struct DenseMatrix {
    map: Mmap,
    costs: Section<i16>,
    left_size: usize,
    right_size: usize,
}

impl DenseMatrix {
    fn open(path: &Path) -> Result<Self, Error> {
        let map = map_file(path)?;
        let [left_size, right_size, _, _] = header_sizes(&map, path, HEADER_BYTES / 2)?;
        if left_size == 0 || right_size == 0 {
            return Err(broken(path, "the matrix has no context ids"));
        }
        if HEADER_BYTES + left_size * right_size * COST_BYTES != map.len() {
            return Err(broken(path, "the file size does not match the header"));
        }
        let costs = Section::new(&map, HEADER_BYTES..map.len())
            .ok_or_else(|| broken(path, "the cost table is misaligned"))?;
        Ok(Self { map, costs, left_size, right_size })
    }

    pub(crate) fn right_column(&self, right_left_id: u16) -> &[i16] {
        let start = self.left_size * usize::from(right_left_id);
        &self.costs.view(&self.map)[start..start + self.left_size]
    }
}

pub(crate) struct HotMatrix {
    map: Mmap,
    costs: Section<i16>,
    templates: MappedTemplates,
    hot_left: usize,
    hot_right: usize,
}

pub(crate) struct HotColumn<'m> {
    cells: &'m [i16],
    templates: &'m MappedTemplates,
    right_left_id: u16,
}

#[cold]
#[inline(never)]
fn cold_cost(templates: &MappedTemplates, left_right_id: u16, right_left_id: u16) -> i16 {
    templates.cost(left_right_id, right_left_id)
}

impl<'m> HotColumn<'m> {
    pub(crate) fn into_cost_fn(self) -> impl FnMut(u16) -> i16 + 'm {
        let Self { cells, templates, right_left_id } = self;
        move |left_right_id| match cells.get(usize::from(left_right_id)) {
            Some(&cost) => cost,
            None => cold_cost(templates, left_right_id, right_left_id),
        }
    }

    pub(crate) fn cells(&self) -> &'m [i16] {
        self.cells
    }
}

impl HotMatrix {
    fn open(path: &Path, templates: MappedTemplates) -> Result<Self, Error> {
        let map = map_file(path)?;
        let [left_size, right_size, hot_left, hot_right] = header_sizes(&map, path, HOT_HEADER_BYTES / 2)?;
        if left_size != templates.left_size() || right_size != templates.right_size() {
            return Err(broken(path, "the hot matrix size does not match the templates"));
        }
        if hot_left == 0 || hot_right == 0 || hot_left > left_size || hot_right > right_size {
            return Err(broken(path, "the hot block lies outside of the matrix"));
        }
        if HOT_HEADER_BYTES + hot_left * hot_right * COST_BYTES != map.len() {
            return Err(broken(path, "the file size does not match the header"));
        }
        let costs = Section::new(&map, HOT_HEADER_BYTES..map.len())
            .ok_or_else(|| broken(path, "the cost table is misaligned"))?;
        Ok(Self { map, costs, templates, hot_left, hot_right })
    }

    pub(crate) fn hot_left(&self) -> usize {
        self.hot_left
    }

    pub(crate) fn right_column(&self, right_left_id: u16) -> HotColumn<'_> {
        let row = usize::from(right_left_id);
        let cells = if row < self.hot_right {
            &self.costs.view(&self.map)[row * self.hot_left..(row + 1) * self.hot_left]
        } else {
            &[]
        };
        HotColumn { cells, templates: &self.templates, right_left_id }
    }
}

pub(crate) enum Matrix {
    Dense(DenseMatrix),
    Hot(HotMatrix),
}

impl Matrix {
    pub(crate) fn open_dense(path: &Path) -> Result<Self, Error> {
        DenseMatrix::open(path).map(Self::Dense)
    }

    pub(crate) fn open_hot(path: &Path, templates_path: &Path) -> Result<Self, Error> {
        let templates = MappedTemplates::open(templates_path)?;
        HotMatrix::open(path, templates).map(Self::Hot)
    }

    pub(crate) fn left_size(&self) -> usize {
        match self {
            Self::Dense(dense) => dense.left_size,
            Self::Hot(hot) => hot.templates.left_size(),
        }
    }

    pub(crate) fn right_size(&self) -> usize {
        match self {
            Self::Dense(dense) => dense.right_size,
            Self::Hot(hot) => hot.templates.right_size(),
        }
    }
}
