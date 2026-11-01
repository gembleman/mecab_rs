use std::io::Write;
use std::path::Path;

use super::Generator;
use crate::build::model::CutFeature;
use crate::connection_templates::to_cost;
use crate::learn::LineEnds;
use crate::tool_error::{io_error, learn_error};
use mecab_rs::__private::files::{MATRIX_DEF_FILE, write_file_with};
use mecab_rs::Error;
use mecab_rs::IoAction;

fn progress_bar(
    log: &mut dyn Write,
    message: &str,
    current: usize,
    total: usize,
    previous: &mut i32,
) {
    const BAR: &str = "###########################################";
    let scale = BAR.len() as i32;
    let percentage = (100.0 * current as f64 / total as f64) as i32;
    let bar_len = (1.0 * current as f64 * f64::from(scale) / total as f64) as i32;
    if *previous != percentage {
        let shown = (bar_len.max(0) as usize).min(BAR.len());
        let padding = (scale - bar_len).unsigned_abs() as usize;
        let _ = write!(
            log,
            "{message}: {percentage:3}% |{}{}| ",
            &BAR[..shown],
            " ".repeat(padding)
        );
        let _ = write!(log, "{}", if percentage == 100 { "\n" } else { "\r" });
        let _ = log.flush();
    }
    *previous = percentage;
}

impl Generator<'_> {
    pub(super) fn emit_matrix(&self, log: &mut dyn Write) -> Result<(), Error> {
        let matrix = self.out_dir.join(MATRIX_DEF_FILE);
        let mut previous = 0;
        write_file_with(&matrix, |file| -> Result<(), Error> {
            let mut file = LineEnds::new(file, self.ending);
            self.check_ids_present()?;
            writeln!(file, "{} {}", self.ids.right.len(), self.ids.left.len())
                .map_err(io_error(IoAction::Write, &matrix))?;
            self.write_matrix_rows(&mut file, &matrix, log, &mut previous)
        })
    }

    fn check_ids_present(&self) -> Result<(), Error> {
        if self.ids.left.is_empty() {
            return Err(learn_error(self.dic_dir, "left id size is empty"));
        }
        if self.ids.right.is_empty() {
            return Err(learn_error(self.dic_dir, "right id size is empty"));
        }
        Ok(())
    }

    fn write_matrix_rows(
        &self,
        file: &mut impl Write,
        matrix: &Path,
        log: &mut dyn Write,
        previous: &mut i32,
    ) -> Result<(), Error> {
        let plan = self.model.bigram_plan();
        let left_cuts: Vec<CutFeature> = self
            .ids
            .left
            .keys()
            .map(|key| CutFeature::cut(key))
            .collect();
        let mut key = Vec::new();
        let mut row = 0;
        for (right_key, right_id) in &self.ids.right {
            let right_cut = CutFeature::cut(right_key);
            row += 1;
            progress_bar(
                log,
                "emitting matrix      ",
                row + 1,
                self.ids.right.len(),
                previous,
            );
            for (left_cut, left_id) in left_cuts.iter().zip(self.ids.left.values()) {
                let weight = self.model.connection_cost_cut(
                    &plan,
                    &right_cut,
                    left_cut,
                    &mut key,
                    &self.feature_def,
                )?;
                writeln!(
                    file,
                    "{right_id} {left_id} {}",
                    to_cost(weight, f64::from(self.factor))
                )
                .map_err(io_error(IoAction::Write, matrix))?;
            }
        }
        Ok(())
    }
}
