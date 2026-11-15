//! The bound on a trapezoidal state-variable section driven by a bounded input while its
//! integrator gain moves anywhere in `[0, max_gain]` at every sample: the figures a test
//! holds a crossover's states and outputs to.
//!
//! The derivation is in the docs of `tutti-dsp`'s crossover state (`crossover/state.rs`: a
//! Lyapunov function `s^T P s` of the integrators' states `s`, with `P = [[1, e], [e, 1 + k
//! e]]`, `e = 3/8`, `A = [[-k, -1], [1, 0]]`); this computes its constants in closed form
//! from those matrices. `k` is the Butterworth damping `sqrt 2`, restated here because this
//! crate does not depend on `tutti-dsp` (which dev-depends on it) and the constant is not
//! public API.

/// What a section's state and taps are bounded by, in multiples of its input's bound.
#[derive(Clone, Copy, Debug)]
pub struct TptBound {
    /// The state `[s1, s2]`'s Euclidean norm.
    pub state: f64,
    /// The band-pass, low-pass, high-pass and all-pass taps (each its own output).
    pub tap: f64,
}

/// The damping of a Butterworth section, `1 / Q`.
const K: f64 = core::f64::consts::SQRT_2;
/// The weight of the Lyapunov function's off-diagonal.
const E: f64 = 3.0 / 8.0;

type M2 = [[f64; 2]; 2];

fn mul(a: M2, b: M2) -> M2 {
    core::array::from_fn(|i| core::array::from_fn(|j| a[i][0] * b[0][j] + a[i][1] * b[1][j]))
}

fn t(a: M2) -> M2 {
    [[a[0][0], a[1][0]], [a[0][1], a[1][1]]]
}

fn inv(a: M2) -> M2 {
    let d = a[0][0] * a[1][1] - a[0][1] * a[1][0];
    [[a[1][1] / d, -a[0][1] / d], [-a[1][0] / d, a[0][0] / d]]
}

/// The eigenvalues of a 2x2 matrix whose eigenvalues are real, smallest first.
fn eigenvalues(a: M2) -> [f64; 2] {
    let tr = a[0][0] + a[1][1];
    let det = a[0][0] * a[1][1] - a[0][1] * a[1][0];
    let root = (tr * tr / 4.0 - det).max(0.0).sqrt();
    [tr / 2.0 - root, tr / 2.0 + root]
}

/// The bound of one section at integrator gains up to `max_gain`.
#[must_use]
pub fn tpt_section_bound(max_gain: f64) -> TptBound {
    let a: M2 = [[-K, -1.0], [1.0, 0.0]];
    let p: M2 = [[1.0, E], [E, 1.0 + K * E]];
    let pa = mul(p, a);
    let q: M2 = [
        [-(pa[0][0] + pa[0][0]), -(pa[0][1] + pa[1][0])],
        [-(pa[1][0] + pa[0][1]), -(pa[1][1] + pa[1][1])],
    ];
    let pinv = inv(p);
    // |A|_P^2 is the largest eigenvalue of P^-1 A^T P A; q the smallest of P^-1 Q.
    let alpha = eigenvalues(mul(pinv, mul(t(a), mul(p, a))))[1].sqrt();
    let q_min = eigenvalues(mul(pinv, q))[0];
    // Q is diagonal here: Q^-1 is its reciprocal.
    let c = 1.0 / q[0][0] + E * E / q[1][1];
    let lambda = eigenvalues(p)[0];
    let g = max_gain;
    let radius = g + 2.0 * (1.0 + g * alpha) * (c / q_min).sqrt();
    let state = radius / lambda.sqrt();
    let v = (radius + g) / lambda.sqrt();
    TptBound {
        state,
        tap: 1.0 + 2.0 * K * v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tan(0.499 pi)`, the largest gain a crossover solves at.
    const G: f64 = 318.308_839_0;

    /// The constants the derivation states for the largest gain: a state under 1.7e3 and a
    /// tap under 5.8e3 times the input, and the constants positive and finite.
    #[test]
    fn the_bound_at_the_largest_gain_is_the_derived_figure() {
        let b = tpt_section_bound(G);
        assert!((1.6e3..1.7e3).contains(&b.state), "{}", b.state);
        assert!((5.6e3..5.8e3).contains(&b.tap), "{}", b.tap);
    }

    /// The bound is positive at gain 0 and grows with the gain.
    #[test]
    fn the_bound_grows_with_the_gain() {
        let (lo, hi) = (tpt_section_bound(0.0), tpt_section_bound(1.0));
        assert!(lo.state > 0.0 && lo.state < hi.state);
        assert!(tpt_section_bound(G).state > hi.state);
    }
}
