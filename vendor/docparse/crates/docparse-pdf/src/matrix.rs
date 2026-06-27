//! 2-D affine transform in PDF's row-vector convention.
//!
//! A PDF matrix `[a b c d e f]` represents
//! ```text
//! | a b 0 |
//! | c d 0 |
//! | e f 1 |
//! ```
//! and points are row vectors: `[x y 1] * M`. Composition `A.mul(B)` applies
//! `A` first, then `B` (`p * (A*B) == (p*A)*B`).

#[derive(Debug, Clone, Copy)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Matrix {
    pub fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    pub fn translate(x: f64, y: f64) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }

    /// `self` applied first, then `other`.
    pub fn mul(&self, o: &Matrix) -> Matrix {
        Matrix {
            a: self.a * o.a + self.b * o.c,
            b: self.a * o.b + self.b * o.d,
            c: self.c * o.a + self.d * o.c,
            d: self.c * o.b + self.d * o.d,
            e: self.e * o.a + self.f * o.c + o.e,
            f: self.e * o.b + self.f * o.d + o.f,
        }
    }

    /// Transform a point.
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Vertical scale factor of the linear part — used to size glyphs.
    pub fn y_scale(&self) -> f64 {
        (self.c * self.c + self.d * self.d).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_neutral() {
        let m = Matrix {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 3.0,
            e: 5.0,
            f: 7.0,
        };
        let r = m.mul(&Matrix::identity());
        assert_eq!((r.a, r.d, r.e, r.f), (2.0, 3.0, 5.0, 7.0));
    }

    #[test]
    fn translate_then_scale_order() {
        // p * (translate(10,0) * scale2) == (p*translate) * scale2
        let scale2 = Matrix {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            e: 0.0,
            f: 0.0,
        };
        let m = Matrix::translate(10.0, 0.0).mul(&scale2);
        assert_eq!(m.apply(0.0, 0.0), (20.0, 0.0));
    }
}
