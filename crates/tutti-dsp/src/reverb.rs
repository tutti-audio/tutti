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
pub(crate) mod spring;

use tutti_units::{SampleRate, Seconds, Tail};

use crate::{DecayTime, Steps};

/// The frames between ramp steps in the reverbs that run in chunks ([`FdnBank::CHUNK`](crate::FdnBank::CHUNK),
/// [`FreeverbBank::CHUNK`](crate::FreeverbBank::CHUNK)).
pub(crate) const CHUNK: usize = 32;

/// The longest ramp of a reverb's gain, in seconds.
const RAMP: Seconds = Seconds::new(0.020);

/// A 20 ms ramp at `rate`, in whole chunks (at least one).
pub(crate) fn ramp(rate: SampleRate) -> Steps {
    Steps::Fixed(ramp_chunks(rate))
}

/// The chunks of a 20 ms ramp at `rate` (at least one).
pub(crate) fn ramp_chunks(rate: SampleRate) -> u32 {
    let chunks = RAMP.to_samples_ceil(rate).get().div_ceil(CHUNK);
    u32::try_from(chunks).unwrap_or(u32::MAX).max(1)
}

/// The tail a decay time is, in whole frames at `rate`: unbounded when it never ends or is not a
/// finite time.
pub(crate) fn tail_of(decay: DecayTime, rate: SampleRate) -> Tail {
    match decay {
        DecayTime::Finite(s) if s.get().is_finite() => Tail::Finite(s.to_samples_ceil(rate)),
        _ => Tail::Unbounded,
    }
}
