//! The Jacobi elliptic functions an elliptic filter design needs, in `f64`: the complete
//! integral `K`, the functions `sn`, `cn`, `dn` and the incomplete integral `F` (the inverse
//! of `sn`), all by the descending Landen transformation (the arithmetic-geometric mean
//! scale), and the modulus of a nome by the theta series.

use core::f64::consts::PI;

/// The longest Landen descent: the mean converges quadratically, so a few steps reach `f64`.
const STEPS: usize = 32;

/// Terms the theta series stops at: for a nome up to 1/2, `q^(n^2)` is below `f64` by n = 8.
const THETA_TERMS: u32 = 64;

/// An elliptic modulus `k` with its complement `k' = sqrt(1 - k^2)`, both held so that
/// neither is found by cancelling the other when it is near 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Modulus {
    k: f64,
    kc: f64,
}

impl Modulus {
    /// The modulus `k`, in `[0, 1]`.
    pub(super) fn new(k: f64) -> Self {
        Self {
            k,
            kc: libm::sqrt((1.0 - k) * (1.0 + k)),
        }
    }

    /// The modulus `k'`: the one the imaginary axis of `k` is read in.
    pub(super) fn complement(self) -> Self {
        Self {
            k: self.kc,
            kc: self.k,
        }
    }

    /// The modulus itself.
    pub(super) fn k(self) -> f64 {
        self.k
    }

    /// The modulus of the nome `q = exp(-pi K' / K)`, in `[0, 1)`:
    /// `k = (theta2 / theta3)^2` and `k' = (theta4 / theta3)^2`.
    pub(super) fn from_nome(q: f64) -> Self {
        // Past 1/2 the series for `k'` cancels and converges slowly: the nome of `k'` is
        // `exp(pi^2 / ln q)`, small there.
        if q > 0.5 {
            return Self::from_nome(libm::exp(PI * PI / libm::log(q))).complement();
        }
        let (mut theta2, mut theta3, mut theta4) = (0.0, 1.0, 1.0);
        let mut sign = 1.0;
        let mut n = 1u32;
        loop {
            // q^(n^2) and q^(n(n-1)) over the same n.
            let square = libm::pow(q, f64::from(n) * f64::from(n));
            let pronic = libm::pow(q, f64::from(n) * f64::from(n - 1));
            sign = -sign;
            theta3 += 2.0 * square;
            theta4 += 2.0 * sign * square;
            theta2 += pronic;
            if (square < 1e-18 && pronic < 1e-18) || n >= THETA_TERMS {
                break;
            }
            n += 1;
        }
        let theta2 = 2.0 * libm::pow(q, 0.25) * theta2;
        Self {
            k: (theta2 / theta3) * (theta2 / theta3),
            kc: (theta4 / theta3) * (theta4 / theta3),
        }
    }
}

/// The complete integral of the first kind, `K(k) = pi / (2 agm(1, k'))`.
pub(super) fn complete_k(m: Modulus) -> f64 {
    let (mut a, mut b) = (1.0, m.kc);
    for _ in 0..STEPS {
        if (a - b).abs() <= 1e-16 * a {
            break;
        }
        (a, b) = (0.5 * (a + b), libm::sqrt(a * b));
    }
    PI / (2.0 * a)
}

/// `sn(u, k)`, `cn(u, k)` and `dn(u, k)` for a real `u`, by the descending Landen
/// transformation: the amplitude `phi = 2^N a_N u` is brought back down the scale.
pub(super) fn sn_cn_dn(u: f64, m: Modulus) -> (f64, f64, f64) {
    let mut a = [0.0; STEPS + 1];
    let mut c = [0.0; STEPS + 1];
    (a[0], c[0]) = (1.0, m.k);
    let mut b = m.kc;
    let mut n = 0;
    while n < STEPS && c[n].abs() > 1e-17 {
        a[n + 1] = 0.5 * (a[n] + b);
        c[n + 1] = 0.5 * (a[n] - b);
        b = libm::sqrt(a[n] * b);
        n += 1;
    }
    let mut phi = libm::ldexp(a[n] * u, n as i32);
    for i in (1..=n).rev() {
        // sin(2 phi' - phi) = (c / a) sin phi with c < a: the principal arcsine is the branch.
        phi = 0.5 * (phi + libm::asin(c[i] / a[i] * libm::sin(phi)));
    }
    let (sn, cn) = (libm::sin(phi), libm::cos(phi));
    // dn^2 = 1 - k^2 sn^2 = k'^2 + k^2 cn^2, with no cancellation near sn = 1.
    (sn, cn, libm::sqrt(m.kc * m.kc + m.k * m.k * cn * cn))
}

/// `cd(u, k) = cn(u, k) / dn(u, k) = sn(u + K, k)`.
pub(super) fn cd(u: f64, m: Modulus) -> f64 {
    let (_, cn, dn) = sn_cn_dn(u, m);
    cn / dn
}

/// The incomplete integral of the first kind `F(phi, k)` for an amplitude `phi` in `[0,
/// pi / 2]`: `u` with `sn(u, k) = sin(phi)`. The same descent run upward on the amplitude.
pub(super) fn incomplete_k(phi: f64, m: Modulus) -> f64 {
    let (mut a, mut b) = (1.0, m.kc);
    let mut phi = phi;
    let mut n = 0;
    while n < STEPS && (a - b).abs() > 1e-16 * a {
        // tan(phi' - phi) = (b / a) tan phi, the step nearest phi itself (b is near a, so
        // phi' is near 2 phi): the angle of (a cos phi, b sin phi) up to whole half turns.
        let step = libm::atan2(b * libm::sin(phi), a * libm::cos(phi));
        phi += step + PI * libm::round((phi - step) / PI);
        (a, b) = (0.5 * (a + b), libm::sqrt(a * b));
        n += 1;
    }
    libm::ldexp(phi / a, -(n as i32))
}
