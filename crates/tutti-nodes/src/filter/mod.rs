//! Filter nodes: state-variable, Moog ladder, parametric EQ band, the Butterworth and
//! Linkwitz-Riley high-pass and low-pass, the matched bell and the tilt, and the
//! Linkwitz-Riley crossover, each a kernel on the one driver. The algorithms are
//! `tutti-dsp`'s.

mod bell;
mod crossover;
mod cut;
mod eq;
pub mod eq_band;
pub mod ladder;
pub mod svf;
mod tilt;

pub use bell::{
    BellFilterConfig, BellFilterControls, BellFilterKernel, BellFilterNode, BellFilterSet,
};
pub(crate) use crossover::Splits;
pub use crossover::{CrossoverKernel, CrossoverNode, CrossoverSet};
pub use cut::{
    CutAlignment, CutFilterConfig, CutFilterControls, CutFilterKernel, CutFilterNode, CutFilterSet,
    MIN_CUT_CUTOFF,
};
pub use eq::{EqControls, EqKernel};
pub use eq_band::EqBandNode;
pub use ladder::{LadderFilterNode, LadderKernel, LadderSet};
pub use svf::{SvfFilterNode, SvfKernel, SvfSet};
pub use tilt::{
    TiltFilterConfig, TiltFilterControls, TiltFilterKernel, TiltFilterNode, TiltFilterSet,
};

use tutti_types::{BusTopology, ChannelLayout, Db, Hz, SampleRate};

/// The lowest center a bell or tilt takes: 20 Hz.
const MIN_EQ_CENTER: Hz = Hz(20.0);

/// The largest gain magnitude a bell or tilt takes: 24 dB.
const MAX_EQ_GAIN: Db = Db(24.0);

/// `center` held within [`MIN_EQ_CENTER`] and 0.99 of the Nyquist of `rate`.
fn eq_center(center: Hz, rate: SampleRate) -> Hz {
    Hz(center
        .get()
        .clamp(MIN_EQ_CENTER.get(), rate.nyquist_scaled(0.99).get()))
}

/// `gain` held within [`MAX_EQ_GAIN`].
fn eq_gain(gain: Db) -> Db {
    Db(gain.get().clamp(-MAX_EQ_GAIN.get(), MAX_EQ_GAIN.get()))
}

/// The bus a filter built for a width runs on: the engine's order for it
/// ([`BusTopology::assumed`]).
///
/// # Panics
///
/// Past 16 channels: a filter is one bus.
fn filter_bus(width: ChannelLayout) -> BusTopology {
    BusTopology::assumed(width).expect("a filter is at most 16 channels wide")
}

#[cfg(test)]
pub(super) mod test_utils {
    use tutti_fixtures::signal;
    use tutti_graph::Node;
    use tutti_types::{Amplitude, Hz, SampleRate, Samples};

    pub use signal::rms;

    /// The frequency of bin `m` of the 8192-frame window the levels are read over: a sine
    /// there has whole cycles in it, so its RMS is exact.
    pub fn bin(m: u32) -> f32 {
        (crate::test_support::RATE.get() * f64::from(m) / 8192.0) as f32
    }

    /// The amplitude of a sine whose samples are `x`.
    pub fn level(x: &[f32]) -> f64 {
        f64::from(rms(x)) * 2f64.sqrt()
    }

    /// `input` through `node` in blocks of 1024.
    pub fn render(node: &mut dyn Node, input: &[f32]) -> Vec<f32> {
        input
            .chunks(1024)
            .flat_map(|c| crate::test_support::drive_block(node, &[c]).remove(0))
            .collect()
    }

    /// The gain of the settled half of a unit sine at `f` through `node`, prepared.
    pub fn gain<N: Node>(node: N, f: f32) -> f64 {
        let mut node = crate::test_support::prepared(node);
        let out = render(&mut node, &generate_sine(f, 48_000.0, 16_384));
        level(&out[8_192..])
    }

    /// `signal::impulse` of `len` frames.
    pub fn impulse(len: usize) -> Vec<f32> {
        signal::impulse(Samples(len))
    }

    /// A mono node over `input`, one frame per call (a block of one). The
    /// node must be prepared.
    pub fn process_mono(node: &mut dyn Node, input: &[f32]) -> Vec<f32> {
        let mut output = vec![0.0f32; input.len()];
        for (i, &sample) in input.iter().enumerate() {
            crate::test_support::tick(node, &[sample], &mut output[i..i + 1]);
        }
        output
    }

    /// `signal::sine` at unit amplitude, for a test that holds bare numbers.
    pub fn generate_sine(freq: f32, sample_rate: f32, num_samples: usize) -> Vec<f32> {
        signal::sine(
            Hz(freq),
            Amplitude(1.0),
            SampleRate::of(f64::from(sample_rate)),
            Samples(num_samples),
        )
    }
}
