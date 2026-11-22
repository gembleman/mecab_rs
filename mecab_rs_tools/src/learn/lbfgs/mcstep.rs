use super::STEP_BOUND_RATIO;

#[derive(Clone, Copy, Default)]
pub(super) struct Bracket {
    pub(super) stx: f64,
    pub(super) fx: f64,
    pub(super) dx: f64,
    pub(super) sty: f64,
    pub(super) fy: f64,
    pub(super) dy: f64,
    pub(super) brackt: bool,
}

impl Bracket {
    pub(super) fn offset_by(self, gtest: f64) -> Self {
        Self {
            fx: self.fx - self.stx * gtest,
            fy: self.fy - self.sty * gtest,
            dx: self.dx - gtest,
            dy: self.dy - gtest,
            ..self
        }
    }

    pub(super) fn restore_offset(self, gtest: f64) -> Self {
        Self {
            fx: self.fx + self.stx * gtest,
            fy: self.fy + self.sty * gtest,
            dx: self.dx + gtest,
            dy: self.dy + gtest,
            ..self
        }
    }

    fn is_frozen(&self, stp: f64, stpmin: f64, stpmax: f64) -> bool {
        self.brackt
            && ((stp <= self.stx.min(self.sty) || stp >= self.stx.max(self.sty))
                || self.dx * (stp - self.stx) >= 0.0
                || stpmax < stpmin)
    }

    fn absorb(&mut self, stp: f64, fp: f64, dp: f64, sgnd: f64) {
        if fp > self.fx {
            self.sty = stp;
            self.fy = fp;
            self.dy = dp;
        } else {
            if sgnd < 0.0 {
                self.sty = self.stx;
                self.fy = self.fx;
                self.dy = self.dx;
            }
            self.stx = stp;
            self.fx = fp;
            self.dx = dp;
        }
    }
}

pub(super) fn mcstep(
    bracket: &mut Bracket,
    stp: &mut f64,
    fp: f64,
    dp: f64,
    stpmin: f64,
    stpmax: f64,
) -> i32 {
    if bracket.is_frozen(*stp, stpmin, stpmax) {
        return 0;
    }

    let sgnd = dp * (bracket.dx / bracket.dx.abs());
    let (info, bound, stpf) = select_candidate(bracket, *stp, fp, dp, sgnd, stpmin, stpmax);
    bracket.absorb(*stp, fp, dp, sgnd);

    let clamped = stpmin.max(stpmax.min(stpf));
    *stp = if bound && bracket.brackt {
        bounded_step(bracket, clamped)
    } else {
        clamped
    };
    info
}

fn select_candidate(
    bracket: &mut Bracket,
    stp: f64,
    fp: f64,
    dp: f64,
    sgnd: f64,
    stpmin: f64,
    stpmax: f64,
) -> (i32, bool, f64) {
    if fp > bracket.fx {
        (1, true, extrapolate_higher_value(bracket, stp, fp, dp))
    } else if sgnd < 0.0 {
        (2, false, extrapolate_opposite_slope(bracket, stp, fp, dp))
    } else if dp.abs() < bracket.dx.abs() {
        (
            3,
            true,
            extrapolate_slope_decrease(bracket, stp, fp, dp, stpmin, stpmax),
        )
    } else {
        (
            4,
            false,
            interpolate_same_slope(bracket, stp, fp, dp, stpmin, stpmax),
        )
    }
}

fn bounded_step(bracket: &Bracket, stp: f64) -> f64 {
    let limit = bracket.stx + (bracket.sty - bracket.stx) * STEP_BOUND_RATIO;
    if bracket.sty > bracket.stx {
        limit.min(stp)
    } else {
        limit.max(stp)
    }
}

#[inline]
fn cubic_theta(fdiff: f64, sdiff: f64, dv: f64, dp: f64) -> f64 {
    fdiff * 3.0 / sdiff + dv + dp
}

#[inline]
fn cubic_scale(theta: f64, dv: f64, dp: f64) -> f64 {
    theta.abs().max(dv.abs()).max(dp.abs())
}

#[inline]
fn cubic_discriminant(theta: f64, scale: f64, dv: f64, dp: f64) -> f64 {
    let d1 = theta / scale;
    d1 * d1 - dv / scale * (dp / scale)
}

#[inline]
fn cubic_gamma(theta: f64, dv: f64, dp: f64) -> f64 {
    let scale = cubic_scale(theta, dv, dp);
    scale * cubic_discriminant(theta, scale, dv, dp).sqrt()
}

#[inline]
fn cubic_gamma_nonneg(theta: f64, dv: f64, dp: f64) -> f64 {
    let scale = cubic_scale(theta, dv, dp);
    scale * 0.0f64.max(cubic_discriminant(theta, scale, dv, dp)).sqrt()
}

fn extrapolate_higher_value(bracket: &mut Bracket, stp: f64, fp: f64, dp: f64) -> f64 {
    let (stx, fx, dx) = (bracket.stx, bracket.fx, bracket.dx);
    let theta = cubic_theta(fx - fp, stp - stx, dx, dp);
    let mut gamma = cubic_gamma(theta, dx, dp);
    if stp < stx {
        gamma = -gamma;
    }
    let p = gamma - dx + theta;
    let q = gamma - dx + gamma + dp;
    let r = p / q;
    let stpc = stx + r * (stp - stx);
    let stpq = stx + dx / ((fx - fp) / (stp - stx) + dx) / 2.0 * (stp - stx);
    bracket.brackt = true;
    if (stpc - stx).abs() < (stpq - stx).abs() {
        stpc
    } else {
        stpc + (stpq - stpc) / 2.0
    }
}

fn extrapolate_opposite_slope(bracket: &mut Bracket, stp: f64, fp: f64, dp: f64) -> f64 {
    let (stx, fx, dx) = (bracket.stx, bracket.fx, bracket.dx);
    let theta = cubic_theta(fx - fp, stp - stx, dx, dp);
    let mut gamma = cubic_gamma(theta, dx, dp);
    if stp > stx {
        gamma = -gamma;
    }
    let p = gamma - dp + theta;
    let q = gamma - dp + gamma + dx;
    let r = p / q;
    let stpc = stp + r * (stx - stp);
    let stpq = stp + dp / (dp - dx) * (stx - stp);
    bracket.brackt = true;
    if (stpc - stp).abs() > (stpq - stp).abs() {
        stpc
    } else {
        stpq
    }
}

fn extrapolate_slope_decrease(
    bracket: &mut Bracket,
    stp: f64,
    fp: f64,
    dp: f64,
    stpmin: f64,
    stpmax: f64,
) -> f64 {
    let (stx, fx, dx) = (bracket.stx, bracket.fx, bracket.dx);
    let theta = cubic_theta(fx - fp, stp - stx, dx, dp);
    let mut gamma = cubic_gamma_nonneg(theta, dx, dp);
    if stp > stx {
        gamma = -gamma;
    }
    let p = gamma - dp + theta;
    let q = gamma + (dx - dp) + gamma;
    let r = p / q;
    let stpc = if r < 0.0 && gamma != 0.0 {
        stp + r * (stx - stp)
    } else if stp > stx {
        stpmax
    } else {
        stpmin
    };
    let stpq = stp + dp / (dp - dx) * (stx - stp);
    let pick_stpc = if bracket.brackt {
        (stp - stpc).abs() < (stp - stpq).abs()
    } else {
        (stp - stpc).abs() > (stp - stpq).abs()
    };
    if pick_stpc { stpc } else { stpq }
}

fn interpolate_same_slope(
    bracket: &mut Bracket,
    stp: f64,
    fp: f64,
    dp: f64,
    stpmin: f64,
    stpmax: f64,
) -> f64 {
    if !bracket.brackt {
        return if stp > bracket.stx { stpmax } else { stpmin };
    }
    let (sty, fy, dy) = (bracket.sty, bracket.fy, bracket.dy);
    let theta = cubic_theta(fp - fy, sty - stp, dy, dp);
    let mut gamma = cubic_gamma(theta, dy, dp);
    if stp > sty {
        gamma = -gamma;
    }
    let p = gamma - dp + theta;
    let q = gamma - dp + gamma + dy;
    let r = p / q;
    stp + r * (sty - stp)
}
