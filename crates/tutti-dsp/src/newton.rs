//! The capped Newton solve a nonlinear kernel runs per sample.
//!
//! [`capped_newton`] solves `f(u) = 0` from a start with exactly [`NEWTON_CAP`] Newton steps:
//! no tolerance is tested and nothing exits early, so the cost per sample is fixed and the
//! output is a function of the inputs alone, which keeps a parallel render bit-identical to a
//! serial one. A kernel states the residual its cap leaves from the convergence of its own
//! equation (a seed's distance from the root, and the Newton recurrence
//! `e' <= (M / 2m) e^2` for `|f''| <= M` and `|f'| >= m`).

use tutti_buffers::Float;

/// The Newton steps every solve takes. Two squares an error twice: a seed within `e` of the
/// root leaves under `e^4 / 8` when `|f''| <= |f'|`, which a kernel with a closed-form seed
/// already has far below the float's rounding; a third step would only repeat the rounding.
pub(crate) const NEWTON_CAP: usize = 2;

/// A root of `f` after [`NEWTON_CAP`] Newton steps from the better of two starts: `seed`, a
/// closed-form guess for this input, and `warm`, the previous sample's solution.
///
/// `f` returns the value and the slope at a point. The start is the one with the smaller
/// `|f|` (the seed when they tie or when `f(warm)` is NaN), so a seed that misses where the
/// signal is slow loses to the solution it follows, and a warm start that a jump left behind
/// loses to the seed. `f` must not have a zero slope on the path.
#[inline]
#[must_use]
pub(crate) fn capped_newton<F: Float>(f: impl Fn(F) -> (F, F), seed: F, warm: F) -> F {
    capped_newton_steps::<F, NEWTON_CAP>(f, seed, warm)
}

/// [`capped_newton`] with `CAP` steps, for a kernel whose seed is not a closed form and which
/// states its own cap with its reason.
#[inline]
#[must_use]
pub(crate) fn capped_newton_steps<F: Float, const CAP: usize>(
    f: impl Fn(F) -> (F, F),
    seed: F,
    warm: F,
) -> F {
    let at_seed = f(seed);
    let at_warm = f(warm);
    let (start, at_start) = if at_seed.0.abs() <= at_warm.0.abs() || at_warm.0.is_nan() {
        (seed, at_seed)
    } else {
        (warm, at_warm)
    };
    steps_from::<F, CAP>(f, start, at_start)
}

/// Exactly `CAP` Newton steps from `start`, with no choice of start: for a kernel whose
/// closed-form seed is the whole starting guess. The cap, like [`NEWTON_CAP`], is the count
/// the kernel's own convergence argument names.
#[inline]
#[must_use]
pub(crate) fn newton_steps<F: Float, const CAP: usize>(f: impl Fn(F) -> (F, F), start: F) -> F {
    let at_start = f(start);
    steps_from::<F, CAP>(f, start, at_start)
}

/// The step loop both forms share: `CAP` steps from `start`, whose value and slope are
/// `at_start`, so the first step does not evaluate `f` again.
#[inline(always)]
fn steps_from<F: Float, const CAP: usize>(
    f: impl Fn(F) -> (F, F),
    start: F,
    at_start: (F, F),
) -> F {
    let mut u = start;
    let (mut value, mut slope) = at_start;
    for step in 0..CAP {
        if step > 0 {
            (value, slope) = f(u);
        }
        u -= value / slope;
    }
    u
}

#[cfg(test)]
mod tests;
