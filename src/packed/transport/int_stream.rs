use std::path::Path;

use crate::Error;
use crate::dict_files::FEATURE_DEF_FILE;
use crate::template_universe::{UniverseSide, read_bigrams, universe_projections};
use crate::wire::fnv1a64;

mod decode;
mod encode;

pub use decode::decode_payload;
pub use encode::encode_payload;

const MAGIC: &[u8; 4] = b"MRIT";
const VERSION: u32 = 1;

impl std::fmt::Debug for IntContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IntContext")
            .field("bigrams", &self.bigrams.len())
            .field("universe", &self.universe.len())
            .finish_non_exhaustive()
    }
}

pub struct IntContext {
    bigrams: Vec<(String, String)>,
    universe: Vec<(UniverseSide, UniverseSide)>,
}

impl IntContext {
    pub fn load(source_dir: &Path, left_id_def: &str, right_id_def: &str) -> Result<Self, Error> {
        let bigrams = read_bigrams(&source_dir.join(FEATURE_DEF_FILE))?;
        Self::new(bigrams, left_id_def, right_id_def, source_dir)
    }

    fn new(
        bigrams: Vec<(String, String)>,
        left_id_def: &str,
        right_id_def: &str,
        path: &Path,
    ) -> Result<Self, Error> {
        let universe = universe_projections(&bigrams, left_id_def, right_id_def, path)?;
        Ok(Self { bigrams, universe })
    }
}

fn projection_digest(universe: &[(UniverseSide, UniverseSide)]) -> u64 {
    let mut bytes = Vec::new();
    for side in universe.iter().flat_map(|(left, right)| [left, right]) {
        bytes.extend_from_slice(&(side.count as u32).to_le_bytes());
        for key in &side.projection {
            bytes.extend_from_slice(&key.to_le_bytes());
        }
    }
    fnv1a64(&bytes)
}
