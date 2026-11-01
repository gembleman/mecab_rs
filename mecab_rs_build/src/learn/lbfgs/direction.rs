use super::{HISTORY_SIZE, Lbfgs, ddot, one_based};

impl Lbfgs {
    pub(super) fn update_direction(&mut self, size: usize, g: &[f64]) {
        let n = size as i32;
        let ys = self.refresh_diag(size);
        self.store_inverse_curvature(size, ys);
        for i in 1..=n {
            self.w[one_based(i)] = -g[one_based(i)];
        }
        let bound = (self.iter - 1).min(HISTORY_SIZE);
        let cp = self.first_loop(size, bound);
        self.apply_diag(size);
        self.second_loop(size, bound, cp);
        self.save_direction(size);
    }

    fn refresh_diag(&mut self, size: usize) -> f64 {
        let (a, b) = (
            one_based(self.iypt + self.npt + 1),
            one_based(self.ispt + self.npt + 1),
        );
        let ys = ddot(size, &self.w[a..], &self.w[b..]);
        let yy = ddot(size, &self.w[a..], &self.w[a..]);
        self.diag.fill(ys / yy);
        ys
    }

    fn store_inverse_curvature(&mut self, size: usize, ys: f64) {
        let n = size as i32;
        let cp = if self.point == 0 {
            HISTORY_SIZE
        } else {
            self.point
        };
        self.w[one_based(n + cp)] = 1.0 / ys;
    }

    fn first_loop(&mut self, size: usize, bound: i32) -> i32 {
        let n = size as i32;
        let mut cp = self.point;
        for _ in 1..=bound {
            cp -= 1;
            if cp == -1 {
                cp = HISTORY_SIZE - 1;
            }
            let sq = ddot(
                size,
                &self.w[one_based(self.ispt + cp * n + 1)..],
                &self.w[..],
            );
            let inmc = n + HISTORY_SIZE + cp + 1;
            let iycn = self.iypt + cp * n;
            self.w[one_based(inmc)] = self.w[one_based(n + cp + 1)] * sq;
            let d = -self.w[one_based(inmc)];
            self.accumulate(size, d, iycn);
        }
        cp
    }

    fn accumulate(&mut self, size: usize, factor: f64, block: i32) {
        for i in 0..size {
            self.w[i] += factor * self.w[one_based(block + 1) + i];
        }
    }

    fn apply_diag(&mut self, size: usize) {
        let n = size as i32;
        for i in 1..=n {
            self.w[one_based(i)] = self.diag[one_based(i)] * self.w[one_based(i)];
        }
    }

    fn second_loop(&mut self, size: usize, bound: i32, mut cp: i32) {
        let n = size as i32;
        for _ in 1..=bound {
            let yr = ddot(
                size,
                &self.w[one_based(self.iypt + cp * n + 1)..],
                &self.w[..],
            );
            let inmc = n + HISTORY_SIZE + cp + 1;
            let beta = self.w[one_based(inmc)] - self.w[one_based(n + cp + 1)] * yr;
            let iscn = self.ispt + cp * n;
            self.accumulate(size, beta, iscn);
            cp += 1;
            if cp == HISTORY_SIZE {
                cp = 0;
            }
        }
    }

    fn save_direction(&mut self, size: usize) {
        let n = size as i32;
        for i in 1..=n {
            self.w[one_based(self.ispt + self.point * n + i)] = self.w[one_based(i)];
        }
    }
}
