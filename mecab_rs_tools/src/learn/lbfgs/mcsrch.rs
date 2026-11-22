use super::mcstep::{Bracket, mcstep};
use super::{
    CURVATURE_TOLERANCE, DECREASE_TOLERANCE, MAX_STEP, MIN_STEP, REQUEST_EVALUATION,
    WIDTH_TOLERANCE, ddot,
};

const HALF: f64 = 0.5;
const WIDTH_SHRINK_RATIO: f64 = 0.66;
const EXTRAPOLATION_FACTOR: f64 = 4.0;
const MAX_EVALUATIONS: i32 = 20;

#[derive(Default)]
pub(super) struct Mcsrch {
    infoc: i32,
    stage1: bool,
    finit: f64,
    dginit: f64,
    dgtest: f64,
    width: f64,
    width1: f64,
    stmin: f64,
    stmax: f64,
    bracket: Bracket,
}

impl Mcsrch {
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the libmecab line search signature"
    )]
    pub(super) fn search(
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
        if *info == REQUEST_EVALUATION {
            *info = self.evaluate(size, f, g, s, stp, nfev);
            if *info != 0 {
                return;
            }
        } else {
            if !self.begin(size, f, g, s, *stp) {
                return;
            }
            *nfev = 0;
            wa[..size].copy_from_slice(&x[..size]);
        }
        *stp = self.prepare_trial(*stp, *nfev);
        write_trial(size, x, wa, *stp, s);
        *info = REQUEST_EVALUATION;
    }

    fn begin(&mut self, size: usize, f: f64, g: &[f64], s: &[f64], stp: f64) -> bool {
        self.infoc = 1;
        if size == 0 || stp <= 0.0 {
            return false;
        }
        self.dginit = ddot(size, g, s);
        if self.dginit >= 0.0 {
            return false;
        }
        self.stage1 = true;
        self.finit = f;
        self.dgtest = DECREASE_TOLERANCE * self.dginit;
        self.width = MAX_STEP - MIN_STEP;
        self.width1 = self.width / HALF;
        self.bracket = Bracket {
            stx: 0.0,
            fx: self.finit,
            dx: self.dginit,
            sty: 0.0,
            fy: self.finit,
            dy: self.dginit,
            brackt: false,
        };
        true
    }

    fn evaluate(
        &mut self,
        size: usize,
        f: f64,
        g: &[f64],
        s: &[f64],
        stp: &mut f64,
        nfev: &mut i32,
    ) -> i32 {
        *nfev += 1;
        let dg = ddot(size, g, s);
        let ftest1 = self.decrease_bound(*stp);
        let code = self.termination_code(f, dg, *stp, ftest1, *nfev);
        if code == 0 {
            self.update_bracket(f, dg, stp, ftest1);
        }
        code
    }

    fn decrease_bound(&self, stp: f64) -> f64 {
        self.finit + stp * self.dgtest
    }

    fn termination_code(&self, f: f64, dg: f64, stp: f64, ftest1: f64, nfev: i32) -> i32 {
        let mut code = 0;
        if self.bracket_unusable(stp) {
            code = 6;
        }
        if self.max_step_accepted(stp, f, dg, ftest1) {
            code = 5;
        }
        if self.min_step_failed(stp, f, dg, ftest1) {
            code = 4;
        }
        if nfev >= MAX_EVALUATIONS {
            code = 3;
        }
        if self.bracket_collapsed() {
            code = 2;
        }
        if self.curvature_satisfied(f, dg, ftest1) {
            code = 1;
        }
        code
    }

    fn bracket_unusable(&self, stp: f64) -> bool {
        self.bracket.brackt && (stp <= self.stmin || stp >= self.stmax || self.infoc == 0)
    }

    fn max_step_accepted(&self, stp: f64, f: f64, dg: f64, ftest1: f64) -> bool {
        stp == MAX_STEP && f <= ftest1 && dg <= self.dgtest
    }

    fn min_step_failed(&self, stp: f64, f: f64, dg: f64, ftest1: f64) -> bool {
        stp == MIN_STEP && (f > ftest1 || dg >= self.dgtest)
    }

    fn bracket_collapsed(&self) -> bool {
        self.bracket.brackt && self.stmax - self.stmin <= WIDTH_TOLERANCE * self.stmax
    }

    fn curvature_satisfied(&self, f: f64, dg: f64, ftest1: f64) -> bool {
        f <= ftest1 && dg.abs() <= CURVATURE_TOLERANCE * (-self.dginit)
    }

    fn update_bracket(&mut self, f: f64, dg: f64, stp: &mut f64, ftest1: f64) {
        if self.stage1
            && f <= ftest1
            && dg >= DECREASE_TOLERANCE.min(CURVATURE_TOLERANCE) * self.dginit
        {
            self.stage1 = false;
        }
        self.infoc = if self.stage1 && f <= self.bracket.fx && f > ftest1 {
            self.shifted_mcstep(f, dg, stp)
        } else {
            mcstep(&mut self.bracket, stp, f, dg, self.stmin, self.stmax)
        };
        if self.bracket.brackt {
            self.track_width(stp);
        }
    }

    fn shifted_mcstep(&mut self, f: f64, dg: f64, stp: &mut f64) -> i32 {
        let gtest = self.dgtest;
        let fm = f - *stp * gtest;
        let dgm = dg - gtest;
        let mut shifted = self.bracket.offset_by(gtest);
        let info = mcstep(&mut shifted, stp, fm, dgm, self.stmin, self.stmax);
        self.bracket = shifted.restore_offset(gtest);
        info
    }

    fn track_width(&mut self, stp: &mut f64) {
        let span = self.bracket.sty - self.bracket.stx;
        if span.abs() >= WIDTH_SHRINK_RATIO * self.width1 {
            *stp = self.bracket.stx + HALF * span;
        }
        self.width1 = self.width;
        self.width = span.abs();
    }

    fn prepare_trial(&mut self, stp: f64, nfev: i32) -> f64 {
        self.set_limits(stp);
        let floored = stp.max(MIN_STEP);
        let clamped = floored.min(MAX_STEP);
        if self.must_restart_from_stx(clamped, nfev) {
            self.bracket.stx
        } else {
            clamped
        }
    }

    fn set_limits(&mut self, stp: f64) {
        if self.bracket.brackt {
            self.stmin = self.bracket.stx.min(self.bracket.sty);
            self.stmax = self.bracket.stx.max(self.bracket.sty);
        } else {
            self.stmin = self.bracket.stx;
            self.stmax = stp + EXTRAPOLATION_FACTOR * (stp - self.bracket.stx);
        }
    }

    fn must_restart_from_stx(&self, stp: f64, nfev: i32) -> bool {
        self.bracket.brackt
            && ((stp <= self.stmin || stp >= self.stmax)
                || nfev >= MAX_EVALUATIONS - 1
                || self.infoc == 0
                || self.stmax - self.stmin <= WIDTH_TOLERANCE * self.stmax)
    }
}

fn write_trial(size: usize, x: &mut [f64], wa: &[f64], stp: f64, s: &[f64]) {
    for j in 0..size {
        x[j] = wa[j] + stp * s[j];
    }
}
