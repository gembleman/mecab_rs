use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use crate::learn::{LineEnding, LineEnds};
use mecab_rs::__private::files::write_file;
use mecab_rs::Error;

#[derive(Default)]
pub(super) struct ContextIds {
    pub(super) left: BTreeMap<Vec<u8>, i32>,
    pub(super) right: BTreeMap<Vec<u8>, i32>,
    pub(super) left_bos: Vec<u8>,
    pub(super) right_bos: Vec<u8>,
}

impl ContextIds {
    pub(super) fn build(&mut self) {
        for (map, bos) in [
            (&mut self.left, &self.left_bos),
            (&mut self.right, &self.right_bos),
        ] {
            for (id, value) in (1..).zip(map.values_mut()) {
                *value = id;
            }
            map.entry(bos.clone()).or_insert(0);
        }
    }

    pub(super) fn save(&self, left: &Path, right: &Path, ending: LineEnding) -> Result<(), Error> {
        for (path, map) in [(left, &self.left), (right, &self.right)] {
            write_file(path, |file| {
                let mut file = LineEnds::new(file, ending);
                for (key, id) in map {
                    write!(file, "{id} ")?;
                    file.write_all(key)?;
                    file.write_all(b"\n")?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}
