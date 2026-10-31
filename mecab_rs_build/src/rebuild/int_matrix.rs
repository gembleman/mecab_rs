use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crate::connection_templates::{ConnectionTemplates, ExactRows};
use mecab_rs::__private::files::{MATRIX_HOT_FILE, MATRIX_ITP_FILE};
use mecab_rs::__private::matrix::{HotShape, IntPart, IntTemplates};
use mecab_rs::Error;

pub const RUNTIME_SHIFT: u32 = 20;

#[allow(
    clippy::float_cmp,
    reason = "the cost factor must be exactly an integer"
)]
fn integer_cost_factor(templates: &ConnectionTemplates, path: &Path) -> Result<u32, Error> {
    let factor = templates.cost_factor();
    if factor >= 1.0 && factor <= f64::from(mecab_rs::__private::matrix::MAX_COST_FACTOR) {
        let integer = factor as u32;
        if f64::from(integer) == factor {
            return Ok(integer);
        }
    }
    Err(Error::rebuild(
        path,
        "the cost-factor is not a usable integer",
    ))
}

pub fn quantize(
    templates: &ConnectionTemplates,
    shift: u32,
    path: &Path,
) -> Result<IntTemplates, Error> {
    let scale = 2f64.powi(shift as i32);
    let mut parts = Vec::with_capacity(templates.parts().len());
    for part in templates.parts() {
        let mut indptr = vec![0u32];
        let mut left_index = Vec::new();
        let mut weight = Vec::new();
        for span in part.indptr.windows(2) {
            for at in span[0] as usize..span[1] as usize {
                let rounded = (part.weight[at] * scale).round();
                if rounded.is_nan() || rounded.abs() > f64::from(i32::MAX) {
                    return Err(Error::rebuild(
                        path,
                        "a weight does not fit the integer grid",
                    ));
                }
                if rounded != 0.0 {
                    left_index.push(part.left_index[at]);
                    weight.push(rounded as i32);
                }
            }
            indptr.push(left_index.len() as u32);
        }
        parts.push(
            IntPart {
                left_count: part.left_count,
                right_count: part.right_count,
                left_projection: part.left_projection.clone(),
                right_projection: part.right_projection.clone(),
                indptr,
                left_index,
                weight,
            }
            .compacted(),
        );
    }
    IntTemplates::new(
        integer_cost_factor(templates, path)?,
        shift,
        templates.left_size(),
        templates.right_size(),
        parts,
        Vec::new(),
    )
}

pub fn find_exceptions(
    exact: &ConnectionTemplates,
    candidate: &IntTemplates,
    threads: usize,
) -> Result<Vec<(u32, i16)>, Error> {
    let (left_size, right_size) = (exact.left_size(), exact.right_size());
    if u32::try_from(left_size * right_size).is_err()
        || candidate.left_size() != left_size
        || candidate.right_size() != right_size
    {
        return Err(Error::Corrupt("the candidate does not fit the matrix"));
    }
    let view = candidate.view();
    let next_row = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                let mut exact_rows = ExactRows::new(exact);
                let mut scratch = view.scratch();
                let mut real = vec![0i16; left_size];
                let mut computed = vec![0i16; left_size];
                let mut local: Vec<(u32, i16)> = Vec::new();
                loop {
                    let row = next_row.fetch_add(1, Ordering::Relaxed);
                    if row >= right_size {
                        break;
                    }
                    exact_rows.render(row, &mut real);
                    view.render_computed_row(row, &mut scratch, &mut computed);
                    for (column, (&a, &b)) in computed.iter().zip(&real).enumerate() {
                        if a != b {
                            local.push(((row * left_size + column) as u32, b));
                        }
                    }
                }
                found
                    .lock()
                    .expect("an exception worker panicked")
                    .extend(local);
            });
        }
    });
    let mut found = found.into_inner().expect("an exception worker panicked");
    found.sort_unstable_by_key(|&(cell, _)| cell);
    Ok(found)
}

pub fn exact_int_templates(
    templates: &ConnectionTemplates,
    shift: u32,
    threads: usize,
    path: &Path,
) -> Result<IntTemplates, Error> {
    let plain = quantize(templates, shift, path)?;
    let exceptions = find_exceptions(templates, &plain, threads)?;
    plain.with_exceptions(exceptions)
}

pub fn write_int_files(
    templates: &IntTemplates,
    dict_dir: &Path,
    hot: Option<HotShape>,
    threads: usize,
) -> Result<(), Error> {
    use mecab_rs::__private::files::write_file;
    let shape =
        super::templates::hot_shape_or_default(hot, templates.left_size(), templates.right_size());
    write_file(&dict_dir.join(MATRIX_ITP_FILE), |writer| {
        templates.write_templates(writer)
    })?;
    write_file(&dict_dir.join(MATRIX_HOT_FILE), |writer| {
        templates.write_hot_matrix(&shape, threads, writer)
    })
}
