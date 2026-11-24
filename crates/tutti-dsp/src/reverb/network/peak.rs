//! The largest magnitude response of a three-band absorption, as an upper bound.
//!
//! An absorption with band gains `[low, mid, high]` and crossover poles `p1`, `p2` has the
//! response `R(w) = high + (mid - high) L2(w) + (low - mid) L1(w)`, `L_p = H_p^2` and
//! `H_p = (1 - p) / (1 - p e^-jw)`. Where the gains do not rise with frequency (`low >= mid >=
//! high`) every term is a nonnegative weight on a response of magnitude at most 1, which is 1
//! at DC: the peak is `low`. Otherwise the response can exceed every gain between the
//! crossovers, and [`peak_bound`] searches for the peak.

use std::f64::consts::PI;

/// A complex number as `(re, im)`.
type C = (f64, f64);

fn mul(a: C, b: C) -> C {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

fn norm(a: C) -> f64 {
    libm::hypot(a.0, a.1)
}

/// The intervals the first pass splits `[0, PI]` into (dense toward DC, where the bands change).
const FIRST_PASS: u32 = 64;
/// The deepest bisection: an interval narrower than `PI / 2^MAX_DEPTH` is returned as it is.
const MAX_DEPTH: u32 = 40;

/// `(H, H', |D|^2)` of the one-pole `p` at `w`, `D = 1 - p e^-jw`.
fn one_pole(p: f64, w: f64) -> (C, C, f64) {
    let d = (1.0 - p * libm::cos(w), p * libm::sin(w));
    let norm2 = d.0 * d.0 + d.1 * d.1;
    let h = ((1.0 - p) * d.0 / norm2, -(1.0 - p) * d.1 / norm2);
    // `H' = -(1 - p) D' / D^2`, `D' = j p e^-jw`, `1 / D^2 = conj(D^2) / |D|^4`.
    let d_prime = (p * libm::sin(w), p * libm::cos(w));
    let d_sq = mul(d, d);
    let k = -(1.0 - p) / (norm2 * norm2);
    let h_prime = mul(d_prime, (d_sq.0 * k, -d_sq.1 * k));
    (h, h_prime, norm2)
}

/// `(R, R')` of the response at `w`.
fn response(c: [f64; 3], pole: [f64; 2], w: f64) -> (C, C) {
    let (high, c2, c1) = (c[0], c[1], c[2]);
    let (h2, h2p, _) = one_pole(pole[1], w);
    let (h1, h1p, _) = one_pole(pole[0], w);
    let (l2, l1) = (mul(h2, h2), mul(h1, h1));
    let (l2p, l1p) = (mul(h2, h2p), mul(h1, h1p));
    (
        (high + c2 * l2.0 + c1 * l1.0, c2 * l2.1 + c1 * l1.1),
        (
            2.0 * (c2 * l2p.0 + c1 * l1p.0),
            2.0 * (c2 * l2p.1 + c1 * l1p.1),
        ),
    )
}

/// A bound on `|R''|` from `w0` on: `|L''| <= 2 (|H'|^2 + |H| |H''|)` with `|H| <= (1-p)/|D|`,
/// `|H'| <= (1-p) p / |D|^2` and `|H''| <= (1-p) (2 p^2 / |D|^3 + p / |D|^2)`, each falling
/// with `w` because `|D|^2 = 1 - 2 p cos w + p^2` rises.
fn second_derivative(c: [f64; 3], pole: [f64; 2], w0: f64) -> f64 {
    let term = |p: f64| {
        let d = 1.0 - 2.0 * p * libm::cos(w0) + p * p;
        let s = libm::sqrt(d);
        let h = (1.0 - p) / s;
        let h1 = (1.0 - p) * p / d;
        let h2 = (1.0 - p) * (2.0 * p * p / (d * s) + p / d);
        2.0 * (h1 * h1 + h * h2)
    };
    c[1].abs() * term(pole[1]) + c[2].abs() * term(pole[0])
}

struct Search {
    c: [f64; 3],
    pole: [f64; 2],
    /// The largest sampled magnitude.
    best: f64,
    /// How far an interval's bound may exceed `best` before it is split.
    tol: f64,
}

impl Search {
    fn eval(&mut self, w: f64) -> (C, C) {
        let r = response(self.c, self.pole, w);
        self.best = self.best.max(norm(r.0));
        r
    }

    /// A bound on `|R|` over `[w0, w1]`, from the Taylor expansion with the second-derivative
    /// bound at either end (`|R(w)| <= max |R0 + R0' t| + D2 t^2 / 2`, the linear part convex
    /// in `t`), bisected while it exceeds `best + tol`.
    fn bound(&mut self, w0: f64, w1: f64, e0: (C, C), e1: (C, C), depth: u32) -> f64 {
        let step = w1 - w0;
        let slack = second_derivative(self.c, self.pole, w0) * step * step / 2.0;
        let left = norm(e0.0).max(norm((e0.0.0 + e0.1.0 * step, e0.0.1 + e0.1.1 * step)));
        let right = norm(e1.0).max(norm((e1.0.0 - e1.1.0 * step, e1.0.1 - e1.1.1 * step)));
        let ub = left.min(right) + slack;
        if ub <= self.best + self.tol || depth >= MAX_DEPTH {
            return ub;
        }
        let mid = 0.5 * (w0 + w1);
        let em = self.eval(mid);
        self.bound(w0, mid, e0, em, depth + 1)
            .max(self.bound(mid, w1, em, e1, depth + 1))
    }
}

/// An upper bound on the largest `|R(w)|` for `w` in `[0, PI]` of the absorption with gains `g =
/// [low, mid, high]` and crossover poles `pole`: exactly `low` where the gains do not rise with
/// frequency, otherwise at most a thousandth of `1 - max(g)` (a millionth when that gain is 1
/// or more) above the sampled peak. The gains are finite and nonnegative and the poles in
/// `(0, 1)`. No allocation: a first pass of 64 intervals, bisected where the bound exceeds the
/// largest sample.
pub(crate) fn peak_bound(g: [f64; 3], pole: [f64; 2]) -> f64 {
    let (low, mid, high) = (g[0], g[1], g[2]);
    if low >= mid && mid >= high {
        return low;
    }
    let top = low.max(mid).max(high);
    let tol = if top < 1.0 { 1e-3 * (1.0 - top) } else { 1e-6 }.max(1e-12);
    let mut search = Search {
        c: [high, mid - high, low - mid],
        pole,
        best: 0.0,
        tol,
    };
    let mut prev = (0.0, search.eval(0.0));
    let mut bound = 0.0f64;
    for k in 1..=FIRST_PASS {
        let w = PI * (f64::from(k) / f64::from(FIRST_PASS)).powi(2);
        let next = search.eval(w);
        bound = bound.max(search.bound(prev.0, w, prev.1, next, 0));
        prev = (w, next);
    }
    bound
}
