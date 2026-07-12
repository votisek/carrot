// -- animation math: curves, springs, the Anim scalar --

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl CubicBezier {
    pub fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> CubicBezier {
        // x control points stay in range so the t-for-x solve converges
        CubicBezier {
            x1: x1.clamp(0.0, 1.0),
            y1,
            x2: x2.clamp(0.0, 1.0),
            y2,
        }
    }

    // one axis of a cubic with P0=0, P3=1
    fn sample(a: f64, b: f64, t: f64) -> f64 {
        let omt = 1.0 - t;
        3.0 * omt * omt * t * a + 3.0 * omt * t * t * b + t * t * t
    }

    pub fn y_for_x(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        if x == 0.0 || x == 1.0 {
            return x;
        }
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        let mut t = x;
        for _ in 0..30 {
            let cur = Self::sample(self.x1, self.x2, t);
            if (cur - x).abs() < 1e-7 {
                break;
            }
            if cur < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) / 2.0;
        }
        Self::sample(self.y1, self.y2, t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
    Bezier(CubicBezier),
}

impl Curve {
    pub fn y(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Curve::Linear => x,
            Curve::EaseOutQuad => 1.0 - (1.0 - x) * (1.0 - x),
            Curve::EaseOutCubic => 1.0 - (1.0 - x).powi(3),
            Curve::EaseOutExpo => {
                if x >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f64.powf(-10.0 * x)
                }
            }
            Curve::Bezier(b) => b.y_for_x(x),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringK {
    pub beta: f64,   // damping / 2m
    pub omega0: f64, // sqrt(k/m)
    pub epsilon: f64,
}

impl SpringK {
    pub fn new(damping_ratio: f64, stiffness: f64, epsilon: f64) -> SpringK {
        // mass = 1: critical damping 2*sqrt(k), so beta = ratio*sqrt(k)
        let omega0 = stiffness.max(1.0).sqrt();
        SpringK {
            beta: damping_ratio.max(0.0) * omega0,
            omega0,
            epsilon,
        }
    }
}

// displacement from target at t seconds given start offset x0 and velocity
// v0: the analytic solution of x'' + 2*beta*x' + omega0^2 * x = 0. the
// branch test uses f32's epsilon - f64's is too tight to ever go critical
pub(crate) fn spring_pos(k: &SpringK, x0: f64, v0: f64, t: f64) -> f64 {
    let env = (-k.beta * t).exp();
    if (k.beta - k.omega0).abs() <= f32::EPSILON as f64 {
        env * (x0 + (k.beta * x0 + v0) * t)
    } else if k.beta < k.omega0 {
        let w1 = (k.omega0 * k.omega0 - k.beta * k.beta).sqrt();
        env * (x0 * (w1 * t).cos() + ((k.beta * x0 + v0) / w1) * (w1 * t).sin())
    } else {
        let w2 = (k.beta * k.beta - k.omega0 * k.omega0).sqrt();
        env * (x0 * (w2 * t).cosh() + ((k.beta * x0 + v0) / w2) * (w2 * t).sinh())
    }
}

pub(crate) fn spring_duration(k: &SpringK, x0: f64, v0: f64) -> f64 {
    let eps = k.epsilon.clamp(1e-7, 0.5);
    if k.beta <= k.omega0 {
        // the e^(-beta t) envelope crossing epsilon
        return -eps.ln() / k.beta.max(1e-6);
    }
    // overdamped decays slower than its envelope; grow from the envelope guess
    let mut t = -eps.ln() / k.beta.max(1e-6);
    for _ in 0..100 {
        if spring_pos(k, x0, v0, t).abs() <= eps * x0.abs().max(1e-9) {
            break;
        }
        t *= 1.5;
    }
    t
}

pub(crate) fn spring_clamped(k: &SpringK, x0: f64, v0: f64) -> f64 {
    // first touch of the target: millisecond stepping, capped at 3s
    let dur = spring_duration(k, x0, v0);
    let eps = k.epsilon.clamp(1e-7, 0.5) * x0.abs().max(1e-9);
    let mut t = 0.0;
    while t < dur.min(3.0) {
        if spring_pos(k, x0, v0, t).abs() <= eps {
            return t;
        }
        t += 0.001;
    }
    dur.min(3.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_endpoints() {
        for c in [
            Curve::Linear,
            Curve::EaseOutQuad,
            Curve::EaseOutCubic,
            Curve::EaseOutExpo,
            Curve::Bezier(CubicBezier::new(0.05, 0.9, 0.1, 1.05)),
        ] {
            assert_eq!(c.y(0.0), 0.0, "{c:?} at 0");
            assert!((c.y(1.0) - 1.0).abs() < 1e-9, "{c:?} at 1");
        }
    }

    #[test]
    fn bezier_diagonal_is_linear() {
        // control points on the diagonal collapse to y = x
        let b = CubicBezier::new(0.3, 0.3, 0.7, 0.7);
        for i in 0..=10 {
            let x = i as f64 / 10.0;
            assert!((b.y_for_x(x) - x).abs() < 1e-4, "x={x}");
        }
    }

    #[test]
    fn bezier_monotone_in_x() {
        let b = CubicBezier::new(0.05, 0.9, 0.1, 1.05);
        let mut prev = b.y_for_x(0.0);
        for i in 1..=1000 {
            let y = b.y_for_x(i as f64 / 1000.0);
            // y may overshoot 1.0 (that's the point of this curve) but the
            // x-solve must be stable and continuous; slope tops out ~18 at
            // the origin, so a 0.001 step moves y at most ~0.02
            assert!(y.is_finite());
            assert!((y - prev).abs() < 0.05);
            prev = y;
        }
    }

    #[test]
    fn spring_critical_monotone() {
        // damping-ratio 1.0, stiffness 800: beta == omega0 == sqrt(800)
        let k = SpringK::new(1.0, 800.0, 0.0001);
        assert!((k.beta - 800f64.sqrt()).abs() < 1e-9);
        let mut prev = spring_pos(&k, 1.0, 0.0, 0.0);
        assert_eq!(prev, 1.0);
        for i in 1..=200 {
            let x = spring_pos(&k, 1.0, 0.0, i as f64 * 0.005);
            assert!(x <= prev + 1e-12, "no overshoot when critically damped");
            assert!(x >= -1e-12);
            prev = x;
        }
    }

    #[test]
    fn spring_under_overshoots_over_does_not() {
        let under = SpringK::new(0.5, 800.0, 0.0001);
        let over = SpringK::new(2.0, 800.0, 0.0001);
        let crossed = (1..=400).any(|i| spring_pos(&under, 1.0, 0.0, i as f64 * 0.0025) < 0.0);
        assert!(crossed, "underdamped must cross the target");
        let crossed = (1..=400).any(|i| spring_pos(&over, 1.0, 0.0, i as f64 * 0.0025) < -1e-9);
        assert!(!crossed, "overdamped must not cross");
    }

    #[test]
    fn spring_durations() {
        let k = SpringK::new(1.0, 800.0, 0.0001);
        // envelope threshold: -ln(eps)/beta
        let expect = -(0.0001f64.ln()) / k.beta;
        let d = spring_duration(&k, 1.0, 0.0);
        assert!((d - expect).abs() < 1e-6);
        // the envelope estimate leaves the critical branch's (1 + beta*t)
        // polynomial as residual: exactly eps*(1 - ln eps). value() snaps
        // to the target at duration, so this never renders
        assert!(spring_pos(&k, 1.0, 0.0, d).abs() <= 0.0001 * (1.0 - 0.0001f64.ln()) * 1.001);
        assert!(spring_clamped(&k, 1.0, 0.0) <= d + 1e-9);
        // underdamped touches target well before it rests
        let u = SpringK::new(0.5, 800.0, 0.0001);
        assert!(spring_clamped(&u, 1.0, 0.0) < spring_duration(&u, 1.0, 0.0));
    }

    #[test]
    fn easing_shapes() {
        // ease-out means faster than linear early on
        assert!(Curve::EaseOutQuad.y(0.3) > 0.3);
        assert!(Curve::EaseOutCubic.y(0.3) > Curve::EaseOutQuad.y(0.3));
        assert!(Curve::EaseOutExpo.y(0.3) > Curve::EaseOutCubic.y(0.3));
        // out-of-range input clamps
        assert_eq!(Curve::Linear.y(-1.0), 0.0);
        assert_eq!(Curve::Linear.y(2.0), 1.0);
    }
}
