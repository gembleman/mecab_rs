use std::path::Path;

use memmap2::Mmap;

use crate::dictionary::map_file;
use crate::error::Error;
use crate::section::Section;

const HEADER_BYTES: usize = 4;
const COST_BYTES: usize = 2;

fn broken(path: &Path, reason: &'static str) -> Error {
    Error::Format { file: path.to_path_buf(), reason }
}

pub(crate) struct Matrix {
    map: Mmap,
    costs: Section<i16>,
    left_size: usize,
    right_size: usize,
}

impl Matrix {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let map = map_file(path)?;
        let Some((&[left_low, left_high, right_low, right_high], _)) = map.split_first_chunk() else {
            return Err(broken(path, "the file is shorter than its header"));
        };
        let left_size = usize::from(u16::from_le_bytes([left_low, left_high]));
        let right_size = usize::from(u16::from_le_bytes([right_low, right_high]));
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

    pub(crate) fn left_size(&self) -> usize {
        self.left_size
    }

    pub(crate) fn right_size(&self) -> usize {
        self.right_size
    }

    pub(crate) fn right_column(&self, right_lc_attr: u16) -> &[i16] {
        let start = self.left_size * usize::from(right_lc_attr);
        &self.costs.view(&self.map)[start..start + self.left_size]
    }
}
