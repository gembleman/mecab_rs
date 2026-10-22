use std::path::Path;

use memmap2::Mmap;

use crate::dictionary::{map_file, mapped_view};
use crate::error::Error;

const HEADER_BYTES: usize = 4;
const COST_BYTES: usize = 2;

fn broken(path: &Path, reason: &'static str) -> Error {
    Error::Format { file: path.to_path_buf(), reason }
}

pub(crate) struct Matrix {
    _map: Mmap,
    costs: &'static [i16],
    left_size: usize,
    right_size: usize,
}

impl Matrix {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let map = map_file(path)?;
        if map.len() < HEADER_BYTES {
            return Err(broken(path, "the file is shorter than its header"));
        }
        let left_size = usize::from(u16::from_le_bytes([map[0], map[1]]));
        let right_size = usize::from(u16::from_le_bytes([map[2], map[3]]));
        if HEADER_BYTES + left_size * right_size * COST_BYTES != map.len() {
            return Err(broken(path, "the file size does not match the header"));
        }
        let costs = mapped_view(&map[HEADER_BYTES..])
            .ok_or_else(|| broken(path, "the cost table is misaligned"))?;
        Ok(Self { _map: map, costs, left_size, right_size })
    }

    pub(crate) fn left_size(&self) -> usize {
        self.left_size
    }

    pub(crate) fn right_size(&self) -> usize {
        self.right_size
    }

    pub(crate) fn right_column(&self, right_lc_attr: u16) -> &[i16] {
        let start = self.left_size * usize::from(right_lc_attr);
        self.costs.get(start..).unwrap_or(&[])
    }
}
