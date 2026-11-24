//! The largest magnitude response of a three-band absorption, as an upper bound.
//!
//! An absorption with band gains `[low, mid, high]` and crossover poles `p1`, `p2` has the
//! response `R(w) = high + (mid - high) L2(w) + (low - mid) L1(w)`, `L_p = H_p^2` and
//! `H_p = (1 - p) / (1 - p e^-jw)`. Where the gains do not rise with frequency (`low >= mid >=
//! high`) every term is a nonnegative weight on a response of magnitude at most 1, which is 1
//! at DC: the peak is `low`. Otherwise the response can exceed every gain between the
//! crossovers, and [`peak_bound`] searches for the peak.

use std::f64::consts::PI;

use tutti_units::Amplitude;

/// A complex number as `(re, im)`.
type C = (f64, f64);

fn mul(a: C, b: C) -> C {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

fn norm(a: C) -> f64 {
    libm::hypot(a.0, a.1)
}

/// The intervals the first pass splits `[0, PI]` into (dense toward DC, where the bands change).
const FIRST_PASS: usize = 16;
/// The deepest bisection: an interval narrower than `PI / 2^MAX_DEPTH` is returned as it is.
const MAX_DEPTH: u32 = 40;

/// `(H, H')` of the one-pole `p` at the angle whose cosine and sine are `cos` and `sin`,
/// `D = 1 - p e^-jw`.
fn one_pole(p: f64, cos: f64, sin: f64) -> (C, C) {
    let d = (1.0 - p * cos, p * sin);
    let norm2 = d.0 * d.0 + d.1 * d.1;
    let h = ((1.0 - p) * d.0 / norm2, -(1.0 - p) * d.1 / norm2);
    // `H' = -(1 - p) D' / D^2`, `D' = j p e^-jw`, `1 / D^2 = conj(D^2) / |D|^4`.
    let d_prime = (p * sin, p * cos);
    let d_sq = mul(d, d);
    let k = -(1.0 - p) / (norm2 * norm2);
    (h, mul(d_prime, (d_sq.0 * k, -d_sq.1 * k)))
}

/// The response at one angle: its cosine, `R` and `R'`.
#[derive(Clone, Copy)]
struct Point {
    cos: f64,
    r: C,
    r_prime: C,
}

fn response(c: [f64; 3], pole: [f64; 2], w: f64) -> Point {
    let (high, c2, c1) = (c[0], c[1], c[2]);
    let (cos, sin) = (libm::cos(w), libm::sin(w));
    let (h2, h2p) = one_pole(pole[1], cos, sin);
    let (h1, h1p) = one_pole(pole[0], cos, sin);
    let (l2, l1) = (mul(h2, h2), mul(h1, h1));
    let (l2p, l1p) = (mul(h2, h2p), mul(h1, h1p));
    Point {
        cos,
        r: (high + c2 * l2.0 + c1 * l1.0, c2 * l2.1 + c1 * l1.1),
        r_prime: (
            2.0 * (c2 * l2p.0 + c1 * l1p.0),
            2.0 * (c2 * l2p.1 + c1 * l1p.1),
        ),
    }
}

/// A bound on `|R''|` from the angle of cosine `cos` on: `|L''| <= 2 (|H'|^2 + |H| |H''|)` with
/// `|H| <= (1-p)/|D|`, `|H'| <= (1-p) p / |D|^2` and `|H''| <= (1-p) (2 p^2 / |D|^3 +
/// p / |D|^2)`, each falling with `w` because `|D|^2 = 1 - 2 p cos w + p^2` rises.
fn second_derivative(c: [f64; 3], pole: [f64; 2], cos: f64) -> f64 {
    let term = |p: f64| {
        let d = 1.0 - 2.0 * p * cos + p * p;
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
    fn eval(&mut self, w: f64) -> Point {
        let p = response(self.c, self.pole, w);
        self.best = self.best.max(norm(p.r));
        p
    }

    /// A bound on `|R|` over `[w0, w1]`, from the Taylor expansion with the second-derivative
    /// bound at either end (`|R(w)| <= max |R0 + R0' t| + D2 t^2 / 2`, the linear part convex
    /// in `t`), bisected while it exceeds `best + tol`.
    fn bound(&mut self, w0: f64, w1: f64, e0: Point, e1: Point, depth: u32) -> f64 {
        let step = w1 - w0;
        let slack = second_derivative(self.c, self.pole, e0.cos) * step * step / 2.0;
        let left = norm(e0.r).max(norm((
            e0.r.0 + e0.r_prime.0 * step,
            e0.r.1 + e0.r_prime.1 * step,
        )));
        let right = norm(e1.r).max(norm((
            e1.r.0 - e1.r_prime.0 * step,
            e1.r.1 - e1.r_prime.1 * step,
        )));
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
    let angle = |k: usize| PI * (k as f64 / FIRST_PASS as f64).powi(2);
    let mut points = [search.eval(0.0); FIRST_PASS + 1];
    for (k, point) in points.iter_mut().enumerate().skip(1) {
        *point = search.eval(angle(k));
    }
    // The bound of each interval is held against the largest of all the samples.
    let mut bound = 0.0f64;
    for k in 0..FIRST_PASS {
        bound = bound.max(search.bound(angle(k), angle(k + 1), points[k], points[k + 1], 0));
    }
    bound
}

/// The gains `g = [low, mid, high]` of a three-band absorption whose one-poles are `pole`
/// (`exp(-2 pi fc / fs)` for the two crossovers), scaled so that no frequency decays slower
/// than the slowest band, and a bound of the scaled response's peak.
///
/// Where the response between the crossovers peaks above the largest gain `M` (bands that
/// rise with frequency, whose one-poles' phase adds to a bump), all three gains are multiplied
/// by `min(M, 0.9999) / peak`, `peak` the [`peak_bound`]: the true peak is at most
/// `min(M, 0.9999)`, which is the bound returned, and the other bands decay a little faster
/// than set. Gains that do not rise with frequency peak at the low band, which is `M`: they are
/// returned as they are with `M`. NaN, infinite or negative gains are silence.
///
/// The design runs it ([`Absorption::designed`](super::Absorption)) and the network carries the
/// result, which the bank reads as it is: the gains a network has are the ones the bank runs.
pub(crate) fn rescale_bands(g: [Amplitude; 3], pole: [f64; 2]) -> ([Amplitude; 3], f64) {
    if !g.iter().all(|x| x.get().is_finite() && x.get() >= 0.0) {
        return ([Amplitude::SILENT; 3], 0.0);
    }
    let top = g.iter().map(|x| x.get()).fold(0.0, f64::max);
    let peak = peak_bound(g.map(Amplitude::get), pole);
    if peak > top {
        let held = top.min(0.9999);
        (g.map(|x| x * (held / peak)), held)
    } else {
        (g, peak)
    }
}
