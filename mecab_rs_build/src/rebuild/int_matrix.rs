use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::bad;
use mecab_rs::__private::ExactRows;
use mecab_rs::__private::{ConnectionTemplates, IntPart, IntTemplates};
use mecab_rs::{Error, HotShape};

pub const RUNTIME_SHIFT: u32 = 20;

fn integer_cost_factor(templates: &ConnectionTemplates, path: &Path) -> Result<u32, Error> {
    let factor = templates.cost_factor();
    if factor >= 1.0 && factor <= f64::from(mecab_rs::__private::MAX_COST_FACTOR) {
        let integer = factor as u32;
        if f64::from(integer) == factor {
            return Ok(integer);
        }
    }
    Err(bad(path, "the cost-factor is not a usable integer"))
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
                    return Err(bad(path, "a weight does not fit the integer grid"));
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
    candidates: &[IntTemplates],
    threads: usize,
) -> Result<Vec<Vec<(u32, i16)>>, Error> {
    let (left_size, right_size) = (exact.left_size(), exact.right_size());
    if u32::try_from(left_size * right_size).is_err()
        || candidates
            .iter()
            .any(|c| c.left_size() != left_size || c.right_size() != right_size)
    {
        return Err(Error::Corrupt("the candidates do not fit the matrix"));
    }
    let views: Vec<_> = candidates.iter().map(IntTemplates::view).collect();
    let next_row = AtomicUsize::new(0);
    let found = Mutex::new(vec![Vec::new(); candidates.len()]);
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                let mut exact_rows = ExactRows::new(exact);
                let mut scratch = views.iter().map(|view| view.scratch()).collect::<Vec<_>>();
                let mut real = vec![0i16; left_size];
                let mut computed = vec![0i16; left_size];
                let mut local: Vec<Vec<(u32, i16)>> = vec![Vec::new(); views.len()];
                loop {
                    let row = next_row.fetch_add(1, Ordering::Relaxed);
                    if row >= right_size {
                        break;
                    }
                    exact_rows.render(row, &mut real);
                    for ((view, scratch), list) in views.iter().zip(&mut scratch).zip(&mut local) {
                        view.render_computed_row(row, scratch, &mut computed);
                        for (column, (&a, &b)) in computed.iter().zip(&real).enumerate() {
                            if a != b {
                                list.push(((row * left_size + column) as u32, b));
                            }
                        }
                    }
                }
                let mut found = found.lock().expect("an exception worker panicked");
                for (all, list) in found.iter_mut().zip(local) {
                    all.extend(list);
                }
            });
        }
    });
    let mut found = found.into_inner().expect("an exception worker panicked");
    for list in &mut found {
        list.sort_unstable_by_key(|&(cell, _)| cell);
    }
    Ok(found)
}

pub fn exact_int_templates(
    templates: &ConnectionTemplates,
    shift: u32,
    threads: usize,
    path: &Path,
) -> Result<IntTemplates, Error> {
    let plain = quantize(templates, shift, path)?;
    let exceptions = find_exceptions(templates, std::slice::from_ref(&plain), threads)?
        .pop()
        .expect("one candidate gives one exception list");
    plain.with_exceptions(exceptions)
}

pub fn write_int_files(
    templates: &IntTemplates,
    dict_dir: &Path,
    hot: Option<HotShape>,
    threads: usize,
) -> Result<(), Error> {
    use mecab_rs::__private::write_file;
    let shape =
        super::templates::hot_shape_or_default(hot, templates.left_size(), templates.right_size());
    write_file(&dict_dir.join("matrix.itp"), |writer| {
        templates.write_templates(writer)
    })?;
    write_file(&dict_dir.join("matrix.hot"), |writer| {
        templates.write_hot_matrix(&shape, threads, writer)
    })
}
