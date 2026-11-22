use super::{CONVERGENCE_TOLERANCE, HISTORY_SIZE, Lbfgs, REQUEST_EVALUATION, ddot, one_based};

impl Lbfgs {
    pub(super) fn step(&mut self, size: usize, x: &mut [f64], f: f64, g: &[f64]) {
        let mut resume = self.iflag == 1;
        if !resume && self.iflag == 0 {
            self.reset_history(size, g);
        }
        loop {
            if !resume {
                self.begin_iteration(size, g);
            }
            resume = false;
            self.line_search(size, x, f, g);
            if !self.settle(size, x, g) {
                return;
            }
        }
    }

    fn reset_history(&mut self, size: usize, g: &[f64]) {
        let n = size as i32;
        self.point = 0;
        self.diag.fill(1.0);
        self.ispt = n + (HISTORY_SIZE << 1);
        self.iypt = self.ispt + n * HISTORY_SIZE;
        for i in 1..=n {
            self.w[one_based(self.ispt + i)] = -g[one_based(i)] * self.diag[one_based(i)];
        }
        self.stp1 = 1.0 / ddot(size, g, g).sqrt();
    }

    fn begin_iteration(&mut self, size: usize, g: &[f64]) {
        self.iter += 1;
        self.info = 0;
        if self.iter != 1 {
            self.update_direction(size, g);
        }
        self.nfev = 0;
        self.stp = if self.iter == 1 { self.stp1 } else { 1.0 };
        let n = size as i32;
        for i in 1..=n {
            self.w[one_based(i)] = g[one_based(i)];
        }
    }

    fn line_search(&mut self, size: usize, x: &mut [f64], f: f64, g: &[f64]) {
        let n = size as i32;
        let Self {
            w,
            diag,
            mcsrch,
            stp,
            info,
            nfev,
            ispt,
            point,
            ..
        } = self;
        let direction = one_based(*ispt + *point * n + 1);
        mcsrch.search(
            size,
            x,
            f,
            g,
            &w[direction..direction + size],
            stp,
            info,
            nfev,
            diag,
        );
    }

    fn settle(&mut self, size: usize, x: &[f64], g: &[f64]) -> bool {
        if self.info == REQUEST_EVALUATION {
            self.iflag = 1;
            return false;
        }
        if self.info != 1 {
            eprintln!(
                "The line search routine mcsrch failed: error code:{}",
                self.info
            );
            self.iflag = -1;
            return false;
        }
        self.record_pair(size, g);
        if converged(size, x, g) {
            self.iflag = 0;
            return false;
        }
        true
    }

    fn record_pair(&mut self, size: usize, g: &[f64]) {
        let n = size as i32;
        self.npt = self.point * n;
        for i in 1..=n {
            self.w[one_based(self.ispt + self.npt + i)] =
                self.stp * self.w[one_based(self.ispt + self.npt + i)];
            self.w[one_based(self.iypt + self.npt + i)] = g[one_based(i)] - self.w[one_based(i)];
        }
        self.point += 1;
        if self.point == HISTORY_SIZE {
            self.point = 0;
        }
    }
}

fn converged(size: usize, x: &[f64], g: &[f64]) -> bool {
    let gnorm = ddot(size, g, g).sqrt();
    let xnorm = 1.0f64.max(ddot(size, x, x).sqrt());
    gnorm / xnorm <= CONVERGENCE_TOLERANCE
}
