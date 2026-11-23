//! Filter nodes: state-variable, Moog ladder, parametric EQ band, the Butterworth and
//! Linkwitz-Riley high-pass and low-pass, the matched bell, the tilt, the graphic equalizer and the
//! Linkwitz-Riley crossover, each a kernel on the one driver. The algorithms are
//! `tutti-dsp`'s.

mod bell;
mod crossover;
mod cut;
mod envelope;
mod eq;
pub mod eq_band;
mod graphic_eq;
pub mod ladder;
mod ramped;
pub mod svf;
mod tilt;

pub use bell::{BellFilterConfig, BellFilterKernel, BellFilterNode, BellFilterSet};
pub(crate) use crossover::Splits;
pub use crossover::{CrossoverConfig, CrossoverKernel, CrossoverNode, CrossoverSet};
pub use cut::{CutFilterConfig, CutFilterKernel, CutFilterNode, CutFilterSet};
pub use envelope::{
    EnvelopeFilterConfig, EnvelopeFilterKernel, EnvelopeFilterNode, EnvelopeFilterSet,
};
pub use eq::{EqControls, EqKernel};
pub use eq_band::{EqBandConfig, EqBandNode};
pub use graphic_eq::{GraphicEqConfig, GraphicEqKernel, GraphicEqNode, GraphicEqSet};
pub use ladder::{LadderFilterConfig, LadderFilterNode, LadderKernel, LadderSet};
pub use svf::{SvfFilterConfig, SvfFilterNode, SvfKernel, SvfSet};
pub use tilt::{TiltFilterConfig, TiltFilterKernel, TiltFilterNode, TiltFilterSet};

use tutti_buffers::{BusTopology, ChannelLayout};

/// The bus a filter built for a width runs on: the engine's order for it
/// ([`BusTopology::default_for`]).
///
/// # Panics
///
/// Past 16 channels: a filter is one bus.
fn filter_bus(width: ChannelLayout) -> BusTopology {
    BusTopology::default_for(width).expect("a filter is at most 16 channels wide")
}

#[cfg(test)]
pub(super) mod test_utils {
    use tutti_fixtures::signal;
    use tutti_graph::Node;
    use tutti_units::{Amplitude, Hz, SampleRate, Samples};

    pub use signal::rms;

    /// The frequency of bin `m` of the 8192-frame window the levels are read over: a sine
    /// there has whole cycles in it, so its RMS is exact.
    pub fn bin(m: u32) -> f64 {
        crate::test_support::RATE.get() * f64::from(m) / 8192.0
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

    /// The first frame from which the tail sum of the `f64` impulse response of `section`
    /// (transposed direct form II, never the kernel's realization) is at most the floor.
    pub fn ring_at(section: &tutti_dsp::Biquad) -> usize {
        let b = section;
        let mut z = [0.0f64; 2];
        let h: Vec<f64> = (0..1 << 21)
            .map(|n| {
                let x = if n == 0 { 1.0 } else { 0.0 };
                let y = b.b0 * x + z[0];
                z[0] = b.b1 * x - b.a1 * y + z[1];
                z[1] = b.b2 * x - b.a2 * y;
                y
            })
            .collect();
        let eps = tutti_dsp::TAIL_FLOOR.to_amplitude().get();
        let mut sum = 0.0;
        for n in (0..h.len()).rev() {
            sum += h[n].abs();
            if sum > eps {
                return n + 1;
            }
        }
        0
    }

    /// The gain of the settled half of a unit sine at `f` through `node`, prepared.
    pub fn gain<N: Node>(node: N, f: f64) -> f64 {
        let mut node = crate::test_support::prepared(node);
        let out = render(&mut node, &generate_sine(f, 48_000.0, 16_384));
        level(&out[8_192..])
    }

    /// `signal::impulse` of `len` frames.
    pub fn impulse(len: usize) -> Vec<f32> {
        signal::impulse(Samples::new(len))
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
    pub fn generate_sine(freq: f64, sample_rate: f32, num_samples: usize) -> Vec<f32> {
        signal::sine(
            Hz::new(freq),
            Amplitude::new(1.0),
            SampleRate::of(f64::from(sample_rate)),
            Samples::new(num_samples),
        )
    }
}
