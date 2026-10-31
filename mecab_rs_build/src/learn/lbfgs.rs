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

#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the libmecab line search signature"
)]
fn mcstep(
    stx: &mut f64,
    fx: &mut f64,
    dx: &mut f64,
    sty: &mut f64,
    fy: &mut f64,
    dy: &mut f64,
    stp: &mut f64,
    fp: f64,
    dp: f64,
    brackt: &mut bool,
    stpmin: f64,
    stpmax: f64,
    info: &mut i32,
) {
    let bound;
    let mut stpf;
    *info = 0;

    if *brackt
        && ((*stp <= stx.min(*sty) || *stp >= stx.max(*sty))
            || *dx * (*stp - *stx) >= 0.0
            || stpmax < stpmin)
    {
        return;
    }

    let sgnd = dp * (*dx / dx.abs());

    if fp > *fx {
        *info = 1;
        bound = true;
        let theta = (*fx - fp) * 3.0 / (*stp - *stx) + *dx + dp;
        let s = theta.abs().max(dx.abs()).max(dp.abs());
        let d1 = theta / s;
        let mut gamma = s * (d1 * d1 - *dx / s * (dp / s)).sqrt();
        if *stp < *stx {
            gamma = -gamma;
        }
        let p = gamma - *dx + theta;
        let q = gamma - *dx + gamma + dp;
        let r = p / q;
        let stpc = *stx + r * (*stp - *stx);
        let stpq = *stx + *dx / ((*fx - fp) / (*stp - *stx) + *dx) / 2.0 * (*stp - *stx);
        if (stpc - *stx).abs() < (stpq - *stx).abs() {
            stpf = stpc;
        } else {
            stpf = stpc + (stpq - stpc) / 2.0;
        }
        *brackt = true;
    } else if sgnd < 0.0 {
        *info = 2;
        bound = false;
        let theta = (*fx - fp) * 3.0 / (*stp - *stx) + *dx + dp;
        let s = theta.abs().max(dx.abs()).max(dp.abs());
        let d1 = theta / s;
        let mut gamma = s * (d1 * d1 - *dx / s * (dp / s)).sqrt();
        if *stp > *stx {
            gamma = -gamma;
        }
        let p = gamma - dp + theta;
        let q = gamma - dp + gamma + *dx;
        let r = p / q;
        let stpc = *stp + r * (*stx - *stp);
        let stpq = *stp + dp / (dp - *dx) * (*stx - *stp);
        if (stpc - *stp).abs() > (stpq - *stp).abs() {
            stpf = stpc;
        } else {
            stpf = stpq;
        }
        *brackt = true;
    } else if dp.abs() < dx.abs() {
        *info = 3;
        bound = true;
        let theta = (*fx - fp) * 3.0 / (*stp - *stx) + *dx + dp;
        let s = theta.abs().max(dx.abs()).max(dp.abs());
        let d3 = theta / s;
        let d2 = d3 * d3 - *dx / s * (dp / s);
        let mut gamma = s * 0.0f64.max(d2).sqrt();
        if *stp > *stx {
            gamma = -gamma;
        }
        let p = gamma - dp + theta;
        let q = gamma + (*dx - dp) + gamma;
        let r = p / q;
        let stpc = if r < 0.0 && gamma != 0.0 {
            *stp + r * (*stx - *stp)
        } else if *stp > *stx {
            stpmax
        } else {
            stpmin
        };
        let stpq = *stp + dp / (dp - *dx) * (*stx - *stp);
        if *brackt {
            if (*stp - stpc).abs() < (*stp - stpq).abs() {
                stpf = stpc;
            } else {
                stpf = stpq;
            }
        } else if (*stp - stpc).abs() > (*stp - stpq).abs() {
            stpf = stpc;
        } else {
            stpf = stpq;
        }
    } else {
        *info = 4;
        bound = false;
        if *brackt {
            let theta = (fp - *fy) * 3.0 / (*sty - *stp) + *dy + dp;
            let s = theta.abs().max(dy.abs()).max(dp.abs());
            let d1 = theta / s;
            let mut gamma = s * (d1 * d1 - *dy / s * (dp / s)).sqrt();
            if *stp > *sty {
                gamma = -gamma;
            }
            let p = gamma - dp + theta;
            let q = gamma - dp + gamma + *dy;
            let r = p / q;
            let stpc = *stp + r * (*sty - *stp);
            stpf = stpc;
        } else if *stp > *stx {
            stpf = stpmax;
        } else {
            stpf = stpmin;
        }
    }

    if fp > *fx {
        *sty = *stp;
        *fy = fp;
        *dy = dp;
    } else {
        if sgnd < 0.0 {
            *sty = *stx;
            *fy = *fx;
            *dy = *dx;
        }
        *stx = *stp;
        *fx = fp;
        *dx = dp;
    }

    stpf = stpmax.min(stpf);
    stpf = stpmin.max(stpf);
    *stp = stpf;
    if *brackt && bound {
        let d1 = *stx + (*sty - *stx) * STEP_BOUND_RATIO;
        if *sty > *stx {
            *stp = d1.min(*stp);
        } else {
            *stp = d1.max(*stp);
        }
    }
}

#[derive(Default)]
struct Mcsrch {
    infoc: i32,
    stage1: bool,
    brackt: bool,
    finit: f64,
    dginit: f64,
    dgtest: f64,
    width: f64,
    width1: f64,
    stx: f64,
    fx: f64,
    dgx: f64,
    sty: f64,
    fy: f64,
    dgy: f64,
    stmin: f64,
    stmax: f64,
}

impl Mcsrch {
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the libmecab line search signature"
    )]
    fn search(
        &mut self,
        size: usize,
        x: &mut [f64],
        f: f64,
        g: &[f64],
        s: &[f64],
        stp: &mut f64,
        info: &mut i32,
        nfev: &mut i32,
        wa: &mut [f64],
    ) {
        const HALF: f64 = 0.5;
        const WIDTH_SHRINK_RATIO: f64 = 0.66;
        const EXTRAPOLATION_FACTOR: f64 = 4.0;
        const MAX_EVALUATIONS: i32 = 20;

        let mut resume = *info == REQUEST_EVALUATION;
        if !resume {
            self.infoc = 1;
            if size == 0 || *stp <= 0.0 {
                return;
            }
            self.dginit = ddot(size, g, s);
            if self.dginit >= 0.0 {
                return;
            }
            self.brackt = false;
            self.stage1 = true;
            *nfev = 0;
            self.finit = f;
            self.dgtest = DECREASE_TOLERANCE * self.dginit;
            self.width = MAX_STEP - MIN_STEP;
            self.width1 = self.width / HALF;
            wa[..size].copy_from_slice(&x[..size]);
            self.stx = 0.0;
            self.fx = self.finit;
            self.dgx = self.dginit;
            self.sty = 0.0;
            self.fy = self.finit;
            self.dgy = self.dginit;
        }

        loop {
            if !resume {
                if self.brackt {
                    self.stmin = self.stx.min(self.sty);
                    self.stmax = self.stx.max(self.sty);
                } else {
                    self.stmin = self.stx;
                    self.stmax = *stp + EXTRAPOLATION_FACTOR * (*stp - self.stx);
                }

                *stp = stp.max(MIN_STEP);
                *stp = stp.min(MAX_STEP);

                if self.brackt
                    && ((*stp <= self.stmin || *stp >= self.stmax)
                        || *nfev >= MAX_EVALUATIONS - 1
                        || self.infoc == 0
                        || self.stmax - self.stmin <= WIDTH_TOLERANCE * self.stmax)
                {
                    *stp = self.stx;
                }

                for j in 0..size {
                    x[j] = wa[j] + *stp * s[j];
                }
                *info = REQUEST_EVALUATION;
                return;
            }
            resume = false;

            *info = 0;
            *nfev += 1;
            let dg = ddot(size, g, s);
            let ftest1 = self.finit + *stp * self.dgtest;

            if self.brackt && ((*stp <= self.stmin || *stp >= self.stmax) || self.infoc == 0) {
                *info = 6;
            }
            if *stp == MAX_STEP && f <= ftest1 && dg <= self.dgtest {
                *info = 5;
            }
            if *stp == MIN_STEP && (f > ftest1 || dg >= self.dgtest) {
                *info = 4;
            }
            if *nfev >= MAX_EVALUATIONS {
                *info = 3;
            }
            if self.brackt && self.stmax - self.stmin <= WIDTH_TOLERANCE * self.stmax {
                *info = 2;
            }
            if f <= ftest1 && dg.abs() <= CURVATURE_TOLERANCE * (-self.dginit) {
                *info = 1;
            }

            if *info != 0 {
                return;
            }

            if self.stage1
                && f <= ftest1
                && dg >= DECREASE_TOLERANCE.min(CURVATURE_TOLERANCE) * self.dginit
            {
                self.stage1 = false;
            }

            if self.stage1 && f <= self.fx && f > ftest1 {
                let fm = f - *stp * self.dgtest;
                let mut fxm = self.fx - self.stx * self.dgtest;
                let mut fym = self.fy - self.sty * self.dgtest;
                let dgm = dg - self.dgtest;
                let mut dgxm = self.dgx - self.dgtest;
                let mut dgym = self.dgy - self.dgtest;
                mcstep(
                    &mut self.stx,
                    &mut fxm,
                    &mut dgxm,
                    &mut self.sty,
                    &mut fym,
                    &mut dgym,
                    stp,
                    fm,
                    dgm,
                    &mut self.brackt,
                    self.stmin,
                    self.stmax,
                    &mut self.infoc,
                );
                self.fx = fxm + self.stx * self.dgtest;
                self.fy = fym + self.sty * self.dgtest;
                self.dgx = dgxm + self.dgtest;
                self.dgy = dgym + self.dgtest;
            } else {
                mcstep(
                    &mut self.stx,
                    &mut self.fx,
                    &mut self.dgx,
                    &mut self.sty,
                    &mut self.fy,
                    &mut self.dgy,
                    stp,
                    f,
                    dg,
                    &mut self.brackt,
                    self.stmin,
                    self.stmax,
                    &mut self.infoc,
                );
            }

            if self.brackt {
                if (self.sty - self.stx).abs() >= WIDTH_SHRINK_RATIO * self.width1 {
                    *stp = self.stx + HALF * (self.sty - self.stx);
                }
                self.width1 = self.width;
                self.width = (self.sty - self.stx).abs();
            }
        }
    }
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
    mcsrch: Option<Mcsrch>,
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

    fn step(&mut self, size: usize, x: &mut [f64], f: f64, g: &[f64]) {
        let msize = HISTORY_SIZE;
        let n = size as i32;

        let one_based = |index: i32| -> usize { (index - 1) as usize };
        let mcsrch = self.mcsrch.get_or_insert_with(Mcsrch::default);
        let diag = &mut self.diag;
        let w = &mut self.w;

        let mut resume = self.iflag == 1;
        if !resume && self.iflag == 0 {
            self.point = 0;
            for value in diag.iter_mut() {
                *value = 1.0;
            }
            self.ispt = n + (msize << 1);
            self.iypt = self.ispt + n * msize;
            for i in 1..=n {
                w[one_based(self.ispt + i)] = -g[one_based(i)] * diag[one_based(i)];
            }
            self.stp1 = 1.0 / ddot(size, g, g).sqrt();
        }

        loop {
            if !resume {
                self.iter += 1;
                self.info = 0;
                if self.iter != 1 {
                    let (a, b) = (
                        one_based(self.iypt + self.npt + 1),
                        one_based(self.ispt + self.npt + 1),
                    );
                    let ys = ddot(size, &w[a..], &w[b..]);
                    let yy = ddot(size, &w[a..], &w[a..]);
                    for value in diag.iter_mut() {
                        *value = ys / yy;
                    }

                    let mut cp = self.point;
                    if self.point == 0 {
                        cp = msize;
                    }
                    w[one_based(n + cp)] = 1.0 / ys;

                    for i in 1..=n {
                        w[one_based(i)] = -g[one_based(i)];
                    }

                    let bound = (self.iter - 1).min(msize);

                    cp = self.point;
                    for _ in 1..=bound {
                        cp -= 1;
                        if cp == -1 {
                            cp = msize - 1;
                        }
                        let sq = ddot(size, &w[one_based(self.ispt + cp * n + 1)..], &w[..]);
                        let inmc = n + msize + cp + 1;
                        let iycn = self.iypt + cp * n;
                        w[one_based(inmc)] = w[one_based(n + cp + 1)] * sq;
                        let d = -w[one_based(inmc)];
                        for i in 0..size {
                            w[i] += d * w[one_based(iycn + 1) + i];
                        }
                    }

                    for i in 1..=n {
                        w[one_based(i)] = diag[one_based(i)] * w[one_based(i)];
                    }

                    for _ in 1..=bound {
                        let yr = ddot(size, &w[one_based(self.iypt + cp * n + 1)..], &w[..]);
                        let mut beta = w[one_based(n + cp + 1)] * yr;
                        let inmc = n + msize + cp + 1;
                        beta = w[one_based(inmc)] - beta;
                        let iscn = self.ispt + cp * n;
                        for i in 0..size {
                            w[i] += beta * w[one_based(iscn + 1) + i];
                        }
                        cp += 1;
                        if cp == msize {
                            cp = 0;
                        }
                    }

                    for i in 1..=n {
                        w[one_based(self.ispt + self.point * n + i)] = w[one_based(i)];
                    }
                }

                self.nfev = 0;
                self.stp = 1.0;
                if self.iter == 1 {
                    self.stp = self.stp1;
                }
                for i in 1..=n {
                    w[one_based(i)] = g[one_based(i)];
                }
            }
            resume = false;

            let direction = one_based(self.ispt + self.point * n + 1);
            mcsrch.search(
                size,
                x,
                f,
                g,
                &w[direction..direction + size],
                &mut self.stp,
                &mut self.info,
                &mut self.nfev,
                diag,
            );
            if self.info == REQUEST_EVALUATION {
                self.iflag = 1;
                return;
            }
            if self.info != 1 {
                eprintln!(
                    "The line search routine mcsrch failed: error code:{}",
                    self.info
                );
                self.iflag = -1;
                return;
            }

            self.npt = self.point * n;
            for i in 1..=n {
                w[one_based(self.ispt + self.npt + i)] =
                    self.stp * w[one_based(self.ispt + self.npt + i)];
                w[one_based(self.iypt + self.npt + i)] = g[one_based(i)] - w[one_based(i)];
            }
            self.point += 1;
            if self.point == msize {
                self.point = 0;
            }

            let gnorm = ddot(size, g, g).sqrt();
            let xnorm = 1.0f64.max(ddot(size, x, x).sqrt());
            if gnorm / xnorm <= CONVERGENCE_TOLERANCE {
                self.iflag = 0;
                return;
            }
        }
    }
}
