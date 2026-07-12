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
