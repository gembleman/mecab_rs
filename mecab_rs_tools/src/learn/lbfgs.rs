mod direction;
mod mcsrch;
mod mcstep;
mod step;

use self::mcsrch::Mcsrch;

const DECREASE_TOLERANCE: f64 = 1e-4;
const WIDTH_TOLERANCE: f64 = 1e-16;
const CONVERGENCE_TOLERANCE: f64 = 1e-7;
const CURVATURE_TOLERANCE: f64 = 0.9;
const MIN_STEP: f64 = 1e-20;
const MAX_STEP: f64 = 1e20;
const HISTORY_SIZE: i32 = 5;
const STEP_BOUND_RATIO: f64 = 0.66;
const REQUEST_EVALUATION: i32 = -1;

fn ddot(size: usize, x: &[f64], y: &[f64]) -> f64 {
    let mut sum = 0.0;
    for i in 0..size {
        sum += x[i] * y[i];
    }
    sum
}

#[inline]
fn one_based(index: i32) -> usize {
    (index - 1) as usize
}

#[derive(Default)]
pub(super) struct Lbfgs {
    iflag: i32,
    nfev: i32,
    point: i32,
    npt: i32,
    iter: i32,
    info: i32,
    ispt: i32,
    iypt: i32,
    stp: f64,
    stp1: f64,
    diag: Vec<f64>,
    w: Vec<f64>,
    mcsrch: Mcsrch,
}

impl Lbfgs {
    fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn optimize(&mut self, size: usize, x: &mut [f64], f: f64, g: &[f64]) -> i32 {
        if self.w.is_empty() {
            self.iflag = 0;
            self.w.resize(
                size * (2 * HISTORY_SIZE as usize + 1) + 2 * HISTORY_SIZE as usize,
                0.0,
            );
            self.diag.resize(size, 0.0);
        } else if self.diag.len() != size {
            eprintln!("size of array is different");
            return -1;
        }

        self.step(size, x, f, g);

        if self.iflag < 0 {
            eprintln!("routine stops with unexpected error");
            return -1;
        }
        if self.iflag == 0 {
            self.clear();
            return 0;
        }
        1
    }
}
