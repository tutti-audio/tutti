//! The band-limited oscillator source: [`OscillatorNode`], a sine, saw, square or
//! triangle at a frequency and a level, made from an [`OscillatorConfig`].
//!
//! The node holds the params, the sample rate and the shape; the algorithm is
//! `tutti_dsp::Oscillator`.

use crate::ramp::LastGood;
use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_dsp::{OscillatorStep, OscillatorWave};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::{Amplitude, Hz, SampleRate, Tail, UnitParam};

/// A band-limited oscillator: a source with no inputs and one (mono) output.
///
/// A sine is exact; a saw and a square are band-limited with PolyBLEP and a
/// triangle with PolyBLAMP (see `tutti_dsp::Oscillator`), so none folds its
/// harmonics back below Nyquist. A frequency outside `0..=Nyquist` is clamped
/// for the step, and one that is not finite holds the phase.
///
/// Its params are the frequency ([`UnitParam::Frequency`], in [`Hz`]) and the level
/// ([`UnitParam::Volume`], in [`Amplitude`], a linear gain on the unit-peak
/// wave), both read per frame: a driven control signal's every frame is heard, and a
/// non-finite driven value holds the last finite one. Made from an
/// [`OscillatorConfig`] (`OscillatorNode::from`); inserted, its controls
/// are a [`ParamSet`]; a fork starts from the values last set through it, at
/// phase 0.
///
/// The node renders silence until the graph prepares it at the device rate
/// ([`Node::prepare`]). It is a source with no ring-out ([`Tail::None`]): what
/// it emits is a function of its clock, so a render's end ends it.
///
/// # Examples
///
/// ```
/// # use tutti_buffers::Planes;
/// use tutti_graph::{GraphBuilder, Params, Prepare};
/// use tutti_nodes::{OscillatorConfig, OscillatorNode, OscillatorWave};
/// use tutti_units::{Amplitude, Hz, SampleRate, Samples};
/// use tutti_buffers::{ChannelLayout, MaxBlock};
///
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
/// let tone = g.add_with_controls(Params(OscillatorNode::from(OscillatorConfig::new(OscillatorWave::Sine, Hz::new(440.0)).level(Amplitude::new(0.5)))));
/// g.pipe_output(tone.0);
/// let mut r = g
///     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
///     .expect("builds");
/// let out = r.render_input(Planes::new(&[&[0.0; 64]]).unwrap());
/// assert_eq!(out[0][0], 0.0);
/// assert!(out[0][1] > 0.0);
/// ```
#[derive(Debug)]
pub struct OscillatorNode {
    wave: tutti_dsp::Oscillator,
    frequency: Param<Hz>,
    level: Param<Amplitude>,
    /// `None` until [`Node::prepare`]: no value is computed from a guessed rate.
    rate: Option<SampleRate>,
    /// The last finite frequency read from the cell or the port.
    good_frequency: LastGood<f64>,
    /// The last finite level read from the cell or the port.
    good_level: LastGood<f64>,
}

// The params an [`OscillatorNode`] lets the graph modulate, in port order.
tutti_graph::declare_params!([] OscillatorNode, n {
    Frequency => Hz = &n.frequency,
    Volume => Amplitude = &n.level,
});

/// What an [`OscillatorNode`] is made from: its wave, its frequency and its level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OscillatorConfig {
    wave: OscillatorWave,
    frequency: Hz,
    level: Amplitude,
}

impl OscillatorConfig {
    /// A `wave` at `frequency`, unity level.
    #[must_use]
    pub const fn new(wave: OscillatorWave, frequency: Hz) -> Self {
        Self {
            wave,
            frequency,
            level: Amplitude::UNITY,
        }
    }

    /// The level the output is scaled by, at the start.
    #[must_use]
    pub const fn level(mut self, level: Amplitude) -> Self {
        self.level = level;
        self
    }
}

impl From<OscillatorConfig> for OscillatorNode {
    fn from(config: OscillatorConfig) -> Self {
        Self {
            wave: tutti_dsp::Oscillator::new(config.wave),
            frequency: Param::new(config.frequency),
            level: Param::new(config.level),
            rate: None,
            good_frequency: LastGood::new(config.frequency.get(), 0.0),
            good_level: LastGood::new(Amplitude::UNITY.get(), 1.0),
        }
    }
}

impl OscillatorNode {
    /// A handle on the frequency cell, for a UI or automation to share.
    #[must_use]
    pub fn frequency(&self) -> Param<Hz> {
        self.frequency.handle()
    }

    /// A handle on the level cell, for a UI or automation to share.
    #[must_use]
    pub fn level(&self) -> Param<Amplitude> {
        self.level.handle()
    }
}

/// Shares the cells (the template [`tutti_graph::param_parts`] forks from).
impl Clone for OscillatorNode {
    fn clone(&self) -> Self {
        Self {
            wave: self.wave,
            frequency: self.frequency.handle(),
            level: self.level.handle(),
            rate: self.rate,
            good_frequency: self.good_frequency,
            good_level: self.good_level,
        }
    }
}

impl<F: Float> Node<F> for OscillatorNode {
    /// No inputs, one output, no latency, no ring-out; arithmetic and one
    /// `sin` a frame, `Light`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_cost(tutti_graph::Cost::Light)
            .with_tail(Tail::None)
            .with_param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.rate = Some(prepare.sample_rate());
    }

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let Some(rate) = self.rate else {
            return Status::Silent;
        };
        let size = io.frames();
        let frequency = self
            .declared_param::<{ UnitParam::Frequency as u16 }, _>(&io)
            .frames();
        let level = self
            .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
            .frames();
        let frequency_cell = self.frequency.load().get();
        let level_cell = self.level.load().get();
        let out = io.output(0);
        // A frequency and a level that hold are read, and cross into `F`, once per block; a
        // control signal's value replaces them at each frame.
        let mut step = OscillatorStep::<F>::new(
            Hz::new(if frequency.is_none() {
                self.good_frequency.read(frequency_cell)
            } else {
                0.0
            }),
            rate,
        );
        let mut gain = if level.is_none() {
            F::from_unit(Amplitude::new(self.good_level.read(level_cell)))
        } else {
            F::ZERO
        };
        for (i, o) in out[..size].iter_mut().enumerate() {
            if let Some(signal) = frequency {
                let f = self.good_frequency.read(signal[i].into());
                step = OscillatorStep::new(Hz::new(f), rate);
            }
            if let Some(signal) = level {
                // A finite control signal value is the level in `F` as it is.
                let good = self.good_level.read(signal[i].into());
                gain = if signal[i].is_finite() {
                    signal[i]
                } else {
                    F::from_unit(Amplitude::new(good))
                };
            }
            *o = self.wave.step(step) * gain;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.wave.reset();
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for OscillatorNode {
    /// The frequency and the level.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with its cells detached (at their values now), at phase 0.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.frequency.detach();
        fork.level.detach();
        Node::<F>::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl<F: Float> IntoNode<F> for OscillatorNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, amplitude_at, mean};
    use tutti_buffers::Planes;
    use tutti_graph::contract::{Direct, assert_param_fork, drive};

    fn render(node: OscillatorNode, frames: usize) -> Vec<f32> {
        let mut d = Direct::new(node, RATE, frames);
        d.block();
        d.output().plane(0).samples().to_vec()
    }

    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(OscillatorNode::from(OscillatorConfig::new(
            OscillatorWave::Saw,
            Hz::new(220.0),
        )));
    }

    #[test]
    fn the_shape_is_a_mono_source_declaring_frequency_then_level() {
        let node = OscillatorNode::from(
            OscillatorConfig::new(OscillatorWave::Sine, Hz::new(440.0)).level(Amplitude::new(0.25)),
        );
        let shape = Node::<f32>::shape(&node);
        assert_eq!(shape.audio_in().count(), 0);
        assert_eq!(shape.audio_out().count(), 1);
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Frequency, UnitParam::Volume][..]
        );
        assert_eq!(Node::<f32>::param_base(&node, 0), Some(440.0));
        assert_eq!(Node::<f32>::param_base(&node, 1), Some(0.25));
    }

    #[test]
    fn the_sine_is_sin_two_pi_f_t_at_the_graph_rate() {
        let out = render(
            OscillatorNode::from(OscillatorConfig::new(OscillatorWave::Sine, Hz::new(440.0))),
            1_000,
        );
        for (n, &y) in out.iter().enumerate() {
            let exact = libm::sin(2.0 * core::f64::consts::PI * 440.0 * n as f64 / 48_000.0);
            assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
        }
    }

    #[test]
    fn level_scales_the_output_exactly() {
        let full = render(
            OscillatorNode::from(OscillatorConfig::new(OscillatorWave::Saw, Hz::new(997.0))),
            512,
        );
        let half = render(
            OscillatorNode::from(
                OscillatorConfig::new(OscillatorWave::Saw, Hz::new(997.0))
                    .level(Amplitude::new(0.5)),
            ),
            512,
        );
        for (a, b) in full.iter().zip(&half) {
            assert_eq!(*b, a * 0.5);
        }
    }

    #[test]
    fn a_square_and_a_saw_have_no_dc_over_whole_periods() {
        for node in [
            OscillatorNode::from(OscillatorConfig::new(
                OscillatorWave::Square,
                Hz::new(1_000.0),
            )),
            OscillatorNode::from(OscillatorConfig::new(OscillatorWave::Saw, Hz::new(1_000.0))),
        ] {
            let out = render(node, 48 * 20);
            assert!(mean(&out).abs() < 1e-6);
        }
    }

    #[test]
    fn a_square_peaks_at_plus_and_minus_one() {
        let out = render(
            OscillatorNode::from(OscillatorConfig::new(
                OscillatorWave::Square,
                Hz::new(1_000.0),
            )),
            48,
        );
        let (hi, lo) = out
            .iter()
            .fold((f32::MIN, f32::MAX), |(h, l), &v| (h.max(v), l.min(v)));
        assert!((hi - 1.0).abs() < 1e-6 && (lo + 1.0).abs() < 1e-6);
        let high = out.iter().filter(|&&v| v > 0.5).count();
        assert_eq!(high, 23);
    }

    /// A 5 kHz saw at 48 kHz lies on 1 kHz lines, and 1 kHz is no harmonic of
    /// it: the naive saw's harmonics 19 and 29 fold onto it at `2/(19 pi)` and
    /// `2/(29 pi)`, each over 0.02; the band-limited one holds under 0.003.
    #[test]
    fn the_saw_does_not_alias_onto_a_non_harmonic_line() {
        let out = render(
            OscillatorNode::from(OscillatorConfig::new(OscillatorWave::Saw, Hz::new(5_000.0))),
            48 * 100,
        );
        assert!(amplitude_at(&out, 1_000.0, 48_000.0) < 0.003);
    }

    #[test]
    fn a_driven_frequency_changes_the_pitch_per_frame() {
        let mut d = Direct::new(
            OscillatorNode::from(OscillatorConfig::new(
                OscillatorWave::Sine,
                Hz::new(1_000.0),
            )),
            RATE,
            96,
        );
        let signal: Vec<f32> = (0..96)
            .map(|i| if i < 48 { 1_000.0 } else { 2_000.0 })
            .collect();
        d.feed(0, Some(&signal));
        d.block();
        let out = d.output().plane(0).samples();
        // 1 kHz for 48 samples is one cycle, so the 2 kHz half starts at phase 0
        // and runs two cycles in the next 48.
        for (n, &y) in out.iter().enumerate().skip(48) {
            let exact =
                libm::sin(2.0 * core::f64::consts::PI * 2_000.0 * (n - 48) as f64 / 48_000.0);
            assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
        }
    }

    #[test]
    fn a_non_finite_driven_value_holds_the_last_finite_one() {
        let mut signal = vec![1_000.0f32; 16];
        signal[8] = f32::NAN;
        signal[9] = f32::INFINITY;
        let mut d = Direct::new(
            OscillatorNode::from(OscillatorConfig::new(OscillatorWave::Sine, Hz::new(500.0))),
            RATE,
            16,
        );
        d.feed(0, Some(&signal));
        d.block();
        for (n, &y) in d.output().plane(0).iter().enumerate() {
            let exact = libm::sin(2.0 * core::f64::consts::PI * 1_000.0 * n as f64 / 48_000.0);
            assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
        }
        let mut level = Direct::new(
            OscillatorNode::from(OscillatorConfig::new(
                OscillatorWave::Sine,
                Hz::new(1_000.0),
            )),
            RATE,
            4,
        );
        level.feed(1, Some(&[0.5, f32::NAN, f32::NEG_INFINITY, 0.5]));
        level.block();
        let held = level.output().plane(0).samples()[1];
        assert!(held.is_finite());
        assert!(
            level
                .output()
                .plane(0)
                .samples()
                .iter()
                .all(|v| v.is_finite())
        );
    }

    #[test]
    fn an_unprepared_node_renders_silence() {
        let mut node =
            OscillatorNode::from(OscillatorConfig::new(OscillatorWave::Sine, Hz::new(440.0)));
        let ones = [1.0f32; 32];
        let out = drive::<f32>(&mut node, RATE, Planes::default(), &[None, Some(&ones)]);
        assert!(out[0].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn reset_restarts_the_cycle() {
        let mut d = Direct::new(
            OscillatorNode::from(OscillatorConfig::new(
                OscillatorWave::Triangle,
                Hz::new(300.0),
            )),
            RATE,
            64,
        );
        d.block();
        let first = d.output().plane(0).samples().to_vec();
        Node::<f32>::reset(&mut *d.node);
        d.block();
        assert_eq!(d.output().plane(0).samples(), &first[..]);
    }

    #[test]
    fn a_set_frequency_is_heard_on_the_next_block() {
        let node = OscillatorNode::from(OscillatorConfig::new(
            OscillatorWave::Sine,
            Hz::new(1_000.0),
        ));
        let cell = node.frequency();
        let mut d = Direct::new(node, RATE, 48);
        d.block();
        cell.store(Hz::new(2_000.0));
        d.block();
        let out = d.output().plane(0).samples();
        // 1 kHz filled one cycle, so the block starts at phase 0 and holds two
        // cycles of 2 kHz.
        let exact = libm::sin(2.0 * core::f64::consts::PI * 2_000.0 * 12.0 / 48_000.0);
        assert!((f64::from(out[12]) - exact).abs() < 1e-6);
    }

    /// In an `f64` graph a saw at `1/64` of the rate at the level `1/3` (which an `f32` cannot
    /// hold) is the ramp `(k / 32 - 1) / 3` of its cycle away from the frames its
    /// band-limiting touches, to `2^-45`: a level or a product narrowed to `f32` is off by
    /// about `2^-25`.
    #[test]
    fn in_an_f64_graph_the_saw_is_the_exact_ramp_of_its_cycle() {
        use crate::test_support::{F64_TOLERANCE, render_f64};
        let level = 1.0 / 3.0;
        let node = OscillatorNode::from(
            OscillatorConfig::new(OscillatorWave::Saw, Hz::new(RATE.get() / 64.0))
                .level(Amplitude::new(level)),
        );
        let out = render_f64(node, RATE, &[], 128);
        for k in 2..62 {
            let want = level * (k as f64 / 32.0 - 1.0);
            assert!((out[0][k] - want).abs() <= F64_TOLERANCE, "frame {k}");
            assert!(
                (out[0][64 + k] - want).abs() <= F64_TOLERANCE,
                "frame {}",
                64 + k
            );
        }
    }
}
