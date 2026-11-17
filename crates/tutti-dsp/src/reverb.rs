//! The reverbs: their model, the [`Network`](network::Network), and the three
//! designs over it, Freeverb, the feedback delay network and the Dattorro plate, each a
//! design and the bank that runs it.
//!
//! The model is a description, and each algorithm owns its realization: an engine that
//! interpreted every [`Network`](network::Network) would fix one loop and one float order
//! for all of them, where each realization keeps its own (Freeverb's is the classic
//! comb and all-pass order). The banks' tests hold a render's RT60, pre-delay, freeze and line
//! lengths to analytic figures, and the node tests hold the reported tail to the network's tail.
//! Algorithms not built here fit the model too: a Schroeder all-pass is a one-line network, early reflections are a
//! [`Taps`](network::Taps) stage, and a convolution is the [`Fir`](network::Stage::Fir)
//! stage's summary.

pub(crate) mod fdn;
pub(crate) mod freeverb;
pub(crate) mod network;
pub(crate) mod plate;

use tutti_types::{SampleRate, Tail};

use crate::{Ring, Steps};

/// The frames between glide steps in the reverbs that run in chunks ([`FdnBank::CHUNK`](crate::FdnBank::CHUNK),
/// [`FreeverbBank::CHUNK`](crate::FreeverbBank::CHUNK)).
pub(crate) const CHUNK: usize = 32;

/// The longest ramp of a reverb's gain, in seconds.
const RAMP: f64 = 0.020;

/// A 20 ms ramp at `rate`, in whole chunks (at least one).
pub(crate) fn ramp(rate: f64) -> Steps {
    Steps::Fixed(((RAMP * rate / CHUNK as f64).ceil() as u32).max(1))
}

/// `buf[(w - delay) mod cap]` interpolated linearly to a fractional `delay` of at least 0:
/// `w` is the slot of the current frame, so delay 0 is the frame just written.
#[inline]
pub(crate) fn read(buf: &[f32], w: usize, delay: f32) -> f32 {
    let cap = buf.len();
    let whole = delay as usize;
    let frac = delay - whole as f32;
    let a = buf[(w + cap - whole) % cap];
    if frac == 0.0 {
        return a;
    }
    let b = buf[(w + cap - whole - 1) % cap];
    crate::lerp(a, b, frac)
}

/// The tail a ring is, in whole frames at `rate`: unbounded when it never ends or is not a
/// finite time.
pub(crate) fn tail_of(ring: Ring, rate: SampleRate) -> Tail {
    match ring {
        Ring::Finite(s) if s.get().is_finite() => Tail::Finite(s.to_samples_ceil(rate)),
        _ => Tail::Unbounded,
    }
}
