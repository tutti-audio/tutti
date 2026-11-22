//! The audio-rate LFO node.
//!
//! [`LfoNode`] owns everything *audio*: the `tutti_graph::Node` impl, the clock
//! position, the sample rate, the beat read from each block's `Env`. It calls
//! [`Waveform::raw_at`] for the one pure step `cycles -> value`, the same
//! function a control lane evaluates, so a beat-synced node and lane produce
//! one sequence (a random wave included). The wave is a pure function of its
//! position in unwrapped cycles: the cycle index addresses a stepped shape's
//! step, the fraction is the phase, and a negative position reads the end of
//! the cycle. A free-running node runs from its insert and does not restart at
//! play.
//!
//! `LfoShape` and `Waveform` are re-exported so `use tutti_nodes::LfoShape`
//! resolves here.

use tutti_units::UnitParam;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::{
    BeatsPerCycle, Bipolar, Hz, PhaseIncrement, PhaseStep, RunningPhase, SampleRate,
};
use tutti_units::{Latency, Tail};

// The waveform vocabulary is tutti-modulation's floor, re-exported so `use
// tutti_nodes::LfoShape` resolves.
pub use tutti_modulation::{LfoShape, Waveform};

/// What an LFO's phase follows: its own frame count, or the transport beat.
///
/// A beat-synced clock's span is part of the clock, set when the node is
/// built and checked there, so no cell holds beats where a rate in [`Hz`]
/// could be written, and a cycle position is always the beat divided by it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LfoClock {
    /// The phase advances from the node's own frame count at the rate in its
    /// rate param ([`UnitParam::Rate`], in [`Hz`]), independent of the
    /// transport, and keeps running while the transport is stopped. A negative
    /// rate runs the phase backwards, and 0 holds it: the value at a position
    /// is a pure function of it, so either is well defined.
    Free,
    /// The phase is read from the transport beat of each frame of the block
    /// ([`tutti_graph::Env::for_each_beat`]), this many beats a cycle: a
    /// larger span is slower. Sample-accurate, and the same offline.
    Synced(BeatsPerCycle),
}

/// The clock's name (`Free Running` or `Beat Synced`), for a UI's selector:
/// display text, not a stable identifier, so do not parse or persist it.
impl core::fmt::Display for LfoClock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Free => "Free Running",
            Self::Synced(_) => "Beat Synced",
        })
    }
}

/// The audio-rate LFO: a graph node (`tutti_graph::Node`, no inputs, one
/// output) that computes a clock position in unwrapped cycles — free-running,
/// or from the transport beat of its block's `Env` — and reads its [`Waveform`]
/// there.
///
/// **The per-sample tier.** Reach for this when a control-rate value is too
/// coarse (audio-rate modulation): an audio generator whose output feeds a
/// node's own audio input. Beat-synced it reads the beat of every frame from
/// its block's `Env`, so it is sample-accurate and renders identically offline.
/// The control-rate alternative is a `tutti_modulation::Source::Lfo` route,
/// which a control lane evaluates into the param's port.
///
/// The depth is a live atomic the UI can write, multiplied onto the wave.
///
/// **Addressable.** The value at a position is a pure function of it, so a
/// random wave steps on the cycle index ([`Waveform::raw_at`]): beat-synced, it
/// replays after a seek; free-running, a fork (which starts at cycle zero)
/// replays the original's sequence from its start.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over the depth ([`UnitParam::Depth`]) and, free-running, the rate
/// ([`UnitParam::Rate`], in [`Hz`]), the one route to them. Both are read
/// per frame, so a driven port's every frame is heard. A beat-synced node's
/// span is its [`LfoClock`], fixed at build. A fork starts from the values
/// last set through the set, at phase zero. It is a source with no ring-out
/// ([`Tail::None`]): what it emits is a function of the clock, so a render's end
/// ends it, and the executor never skips a node with no inputs.
///
/// **Arrival.** It has no inputs, so its compiled arrival is zero by
/// construction and the beat it reads is its block's own; a consumer behind a
/// latent path is aligned by the compiler, which delays this node's edge into
/// it like any other source's.
#[derive(Debug)]
pub struct LfoNode {
    wave: Waveform,
    clock: LfoClock,
    /// The free-running rate: read only by [`LfoClock::Free`], and declared as
    /// a param only then.
    rate: Param<Hz>,
    depth: Param<Bipolar>,
    /// Added to the clock position before the wave reads it.
    phase_offset: PhaseIncrement,
    /// Free-running clock position within its cycle: a [`RunningPhase`], exact for as
    /// long as the node runs.
    phase: RunningPhase,
    /// The whole cycles the free-running clock has completed (negative when it runs
    /// backwards), so a random wave's step index stays exact. The position in unwrapped
    /// cycles is `cycle` plus the phase.
    cycle: i64,
    /// The rate [`Node::prepare`] set; `None` until then, when a free-running
    /// clock has no step and the node renders silence.
    sample_rate: Option<SampleRate>,
}

/// The per-frame step of a free-running clock at `rate`, its magnitude held strictly below
/// half a cycle: a step of half a cycle or more reads as a step back.
fn lfo_step(rate: f64, per_hz: f64) -> PhaseStep {
    let most = 0.5_f64.next_down();
    PhaseStep::from(PhaseIncrement::new((rate * per_hz).clamp(-most, most)))
}

/// What an [`LfoNode`] is made from: its shape, its clock and its starting depth and phase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LfoConfig {
    shape: LfoShape,
    seed: u64,
    clock: LfoClock,
    rate: Hz,
    depth: Bipolar,
    phase_offset: PhaseIncrement,
}

impl LfoConfig {
    /// A free-running LFO of `shape` (seed 0) at a rate of 1 Hz, at full depth and no phase
    /// offset. Its phase step takes the rate [`Node::prepare`] hands the node.
    #[must_use]
    pub const fn new(shape: LfoShape) -> Self {
        Self {
            shape,
            seed: 0,
            clock: LfoClock::Free,
            rate: Hz::new(1.0),
            depth: Bipolar::MAX,
            phase_offset: PhaseIncrement::new(0.0),
        }
    }

    /// The random shapes' seed: seed 0 is the plain sequence, the one a
    /// [`tutti_modulation::Lfo`] of the same seed produces in a control lane.
    /// A shape that is not random ignores it.
    #[must_use]
    pub const fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Runs free at `rate` cycles a second ([`LfoClock::Free`]), at the start.
    ///
    /// The clock moves less than half a cycle a frame: a rate of magnitude `sample rate / 2`
    /// or more runs at just under that, forwards or backwards by its sign, so a random
    /// shape's cycle index never skips or reverses.
    #[must_use]
    pub const fn rate(mut self, rate: Hz) -> Self {
        self.clock = LfoClock::Free;
        self.rate = rate;
        self
    }

    /// Follows the transport beat, `beats_per_cycle` beats a cycle
    /// ([`LfoClock::Synced`]): a larger span is slower.
    #[must_use]
    pub const fn beat_sync(mut self, beats_per_cycle: BeatsPerCycle) -> Self {
        self.clock = LfoClock::Synced(beats_per_cycle);
        self
    }

    /// The modulation [`Bipolar`] at the start.
    ///
    /// Bipolar: a negative depth inverts the wave, so `-1.0` is the same
    /// shape phase-flipped and `0.0` is flat.
    #[must_use]
    pub const fn depth(mut self, depth: Bipolar) -> Self {
        self.depth = depth;
        self
    }

    /// The [`PhaseIncrement`] offset, conventionally `0.0` to `1.0` for one full cycle.
    ///
    /// Added to the unwrapped clock position, so any real value is valid: the
    /// wave takes the phase as the position's `floor`-based fraction, so a
    /// negative offset lags rather than reading off the front of the shape. An
    /// offset past a whole cycle also moves a random wave to a later step.
    #[must_use]
    pub const fn phase_offset(mut self, offset: PhaseIncrement) -> Self {
        self.phase_offset = offset;
        self
    }
}

/// The config for a control-rate LFO's declaration: the same shape, seed,
/// rate and phase. Beat-synced, it produces the values a control lane
/// evaluates from that [`tutti_modulation::Lfo`], at audio rate. Free-running
/// (`Hz`), it runs at that rate from its insert and does not restart
/// at play, where the lane's `Hz` clock is the graph's play clock.
impl From<tutti_modulation::Lfo> for LfoConfig {
    fn from(lfo: tutti_modulation::Lfo) -> Self {
        let config = Self::new(lfo.shape).seed(lfo.seed).phase_offset(lfo.phase);
        match lfo.rate {
            tutti_modulation::LfoRate::Beats(per_cycle) => config.beat_sync(per_cycle),
            tutti_modulation::LfoRate::Hz(rate) => config.rate(rate),
        }
    }
}

impl From<LfoConfig> for LfoNode {
    fn from(config: LfoConfig) -> Self {
        Self {
            wave: Waveform::Shape {
                shape: config.shape,
                seed: config.seed,
            },
            clock: config.clock,
            rate: Param::new(config.rate),
            depth: Param::new(config.depth),
            phase_offset: config.phase_offset,
            phase: RunningPhase::START,
            cycle: 0,
            sample_rate: None,
        }
    }
}

impl LfoNode {
    /// Returns what the phase follows.
    #[must_use]
    pub fn clock(&self) -> LfoClock {
        self.clock
    }

    /// The wave at `cycles` plus the phase offset, with the depth applied.
    #[inline]
    fn evaluate<F: Float>(&self, cycles: f64, offset: PhaseIncrement, depth: F) -> F {
        // The wave is `f64`; it crosses into the output's float once per frame.
        F::from_design(self.wave.raw_at(cycles + offset.get(), false)) * depth
    }
}

// The params an [`LfoNode`] lets the graph modulate, in port order: its
// depth and, free-running, its rate (a beat-synced node's span is its clock).
// Both are read per frame. The clock is fixed once the node is live (a config option
// only), so its shape never changes.
tutti_graph::declare_params!([] LfoNode, n {
    Depth => Bipolar = &n.depth,
    Rate => Hz = matches!(n.clock, LfoClock::Free).then_some(&n.rate),
});

impl<F: Float> Node<F> for LfoNode {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .cost(tutti_graph::Cost::Moderate)
            .tail(Tail::None)
            .param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = Some(p.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        debug_assert_eq!(
            cx.arrival,
            Latency::ZERO,
            "a node with no inputs arrives at zero"
        );
        let size = io.frames();
        // A driven depth is clamped into `-1..=1` (a written one is within it). A
        // non-finite driven frame reads the cell instead: a NaN rate would
        // leave the phase NaN for good. The cell crosses into the output's float once
        // per block.
        let depth_cell = F::from_unit(self.depth.load());
        let depth_signal = self
            .declared_param::<{ UnitParam::Depth as u16 }, _>(&io)
            .frames();
        let depth_at = |i: usize| {
            depth_signal
                .map(|l| l[i])
                .filter(|v| v.is_finite())
                .map_or(depth_cell, |v| v.clamp(F::lit(-1.0), F::lit(1.0)))
        };
        let offset = self.phase_offset;
        match self.clock {
            LfoClock::Free => {
                let rate_cell = self.rate.load().get();
                let rate_signal = self
                    .declared_param::<{ UnitParam::Rate as u16 }, _>(&io)
                    .frames();
                // The step is accumulated in `f64` for as long as the node runs: an
                // `f32`-rounded step drifts the cycle index off the frame.
                let out = io.output(0);
                let Some(sample_rate) = self.sample_rate else {
                    out[..size].fill(F::ZERO);
                    return Status::Modified;
                };
                let per_hz = 1.0 / sample_rate.get();
                // The step is rebuilt only when the rate moves.
                let mut step_of = (rate_cell, lfo_step(rate_cell, per_hz));
                for (i, o) in out[..size].iter_mut().enumerate() {
                    let rate = rate_signal
                        .map(|l| l[i].into())
                        .filter(|v| v.is_finite())
                        .unwrap_or(rate_cell);
                    if rate != step_of.0 {
                        step_of = (rate, lfo_step(rate, per_hz));
                    }
                    let cycles = self.cycle as f64 + f64::from_phase(self.phase);
                    *o = self.evaluate(cycles, offset, depth_at(i));
                    let next = self.phase.advance(step_of.1);
                    // A step above half a cycle is a step back (its two's complement), and
                    // a carry out of the add is a cycle completed, a borrow one undone.
                    if (step_of.1.get() as i64) >= 0 {
                        self.cycle += i64::from(next.get() < self.phase.get());
                    } else {
                        self.cycle -= i64::from(next.get() > self.phase.get());
                    }
                    self.phase = next;
                }
            }
            LfoClock::Synced(beats_per_cycle) => {
                let out = io.output(0);
                cx.env.for_each_beat(|i, beat| {
                    let cycles = beat.cycles_in(beats_per_cycle);
                    out[i] = self.evaluate(cycles, offset, depth_at(i));
                });
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.phase = RunningPhase::START;
        self.cycle = 0;
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for LfoNode {
    /// The depth, and the rate while free-running (a beat-synced node's span is
    /// its clock).
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with every control cell detached (at its value now), at cycle
    /// zero: a free-running fork replays the original's values from its
    /// start.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.rate.detach();
        fork.depth.detach();
        Node::<F>::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts
/// from the values last set through it ([`tutti_graph::param_parts`]).
impl<F: Float> IntoNode<F> for LfoNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

impl Clone for LfoNode {
    fn clone(&self) -> Self {
        Self {
            wave: self.wave.clone(),
            clock: self.clock,
            rate: self.rate.handle(),
            depth: self.depth.handle(),
            phase_offset: self.phase_offset,
            phase: self.phase,
            cycle: self.cycle,
            sample_rate: self.sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::MaxBlock;
    use tutti_graph::contract::{Direct, assert_param_fork};
    use tutti_graph::{Standalone, TransportPosition};
    use tutti_units::UnitParam;
    use tutti_units::{Beat, Bpm, Samples};

    /// A free-running LFO declares its depth and rate, each port's base its
    /// cell; a beat-synced one declares only its depth, its span being its
    /// clock.
    #[test]
    fn the_shape_declares_depth_and_a_free_running_rate() {
        let free = LfoNode::from(
            LfoConfig::new(LfoShape::Sine)
                .rate(Hz::new(2.0))
                .depth(Bipolar::of(0.25)),
        );
        assert_eq!(
            Node::<f32>::shape(&free).params.as_slice(),
            &[UnitParam::Depth, UnitParam::Rate][..]
        );
        assert_eq!(Node::<f32>::param_base(&free, 0), Some(0.25));
        assert_eq!(Node::<f32>::param_base(&free, 1), Some(2.0));
        let synced = LfoNode::from(
            LfoConfig::new(LfoShape::Sine)
                .beat_sync(BeatsPerCycle::of(4.0))
                .depth(Bipolar::of(0.5)),
        );
        assert_eq!(
            Node::<f32>::shape(&synced).params.as_slice(),
            &[UnitParam::Depth][..]
        );
        assert_eq!(Node::<f32>::param_base(&synced, 0), Some(0.5));
        assert_eq!(Node::<f32>::param_base(&synced, 1), None);
    }

    // The waveform math is `tutti-modulation`'s, and so are the tests that pin it.
    // What belongs here is the adapter around it: phase generation from
    // transport, depth, and the node surface.

    /// `n` frames of `node` at `sr` in one block, transport stopped at beat 0.
    fn render(node: LfoNode, sr: f64, n: usize) -> Vec<f32> {
        let mut d = Direct::new(node, SampleRate::of(sr), n);
        d.block();
        d.output().plane(0).samples().to_vec()
    }

    /// `n` frames of `node` through a graph at 48 kHz, each block handed
    /// `transport` as its own (advancing, when it rolls, by the block).
    fn render_at(node: LfoNode, transport: TransportPosition, n: usize) -> Vec<f32> {
        let rate = SampleRate::SR_48K;
        let mut solo = Standalone::new(node, Prepare::new(rate, MaxBlock::of(Samples::new(256))));
        solo.renderer_mut().set_transport_fn(move |frame| {
            let mut clock = tutti_music::FrameClock::new(transport.beat(), transport.tempo, rate);
            if transport.playing {
                clock.advance(Samples::new(frame.get() as usize), None);
            }
            TransportPosition::counted(transport.playing, transport.tempo, clock.origin(), None)
        });
        solo.render(n).remove(0)
    }

    fn stopped_at(beat: f64) -> TransportPosition {
        TransportPosition::new(false, Bpm::of(120.0), Beat::new(beat), None)
    }

    #[test]
    fn test_free_running_lfo() {
        let out = render(LfoNode::from(LfoConfig::new(LfoShape::Sine)), 100.0, 25);
        // The 25th sample: a quarter of a 1 Hz cycle at 100 Hz, the peak.
        assert!(
            (out[24] - 1.0).abs() < 0.1,
            "Expected ~1.0, got {}",
            out[24]
        );
    }

    /// Setting a rate in one clock's unit selects that clock, the last one
    /// set winning: a free rate leaves beat sync behind and a span replaces a
    /// free rate.
    #[test]
    fn setting_a_rate_selects_its_clock() {
        let lfo = LfoNode::from(
            LfoConfig::new(LfoShape::Sine)
                .beat_sync(BeatsPerCycle::of(4.0))
                .rate(Hz::new(2.0)),
        );
        assert_eq!(lfo.clock(), LfoClock::Free);
        assert_eq!(lfo.rate.load(), Hz::new(2.0));
        let lfo = LfoNode::from(
            LfoConfig::new(LfoShape::Sine)
                .rate(Hz::new(2.0))
                .beat_sync(BeatsPerCycle::of(4.0)),
        );
        assert_eq!(lfo.clock(), LfoClock::Synced(BeatsPerCycle::of(4.0)));
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it (see
    /// `tutti_graph::contract::assert_param_fork`). The rate is addressed
    /// only while free-running.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let free = LfoNode::from(LfoConfig::new(LfoShape::Sine).rate(Hz::new(2.0)));
        assert_eq!(
            ParamNode::<f32>::param_set(&free)
                .params()
                .collect::<Vec<_>>(),
            [UnitParam::Depth, UnitParam::Rate]
        );
        assert_param_fork(free);
        let synced =
            LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0)));
        assert_eq!(
            ParamNode::<f32>::param_set(&synced)
                .params()
                .collect::<Vec<_>>(),
            [UnitParam::Depth]
        );
        assert_param_fork(synced);
    }

    /// No inputs, one output, no ring-out: what a clock-driven source emits
    /// ends with the render, so a bounce adds nothing past its span for it.
    #[test]
    fn a_source_has_no_ring_out() {
        for lfo in [
            LfoNode::from(LfoConfig::new(LfoShape::Sine)),
            LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(1.0))),
        ] {
            let shape = Node::<f32>::shape(&lfo);
            assert_eq!(shape.audio_in().count(), 0);
            assert_eq!(shape.audio_out().count(), 1);
            assert_eq!(shape.tail, Tail::None);
        }
    }

    #[test]
    fn test_beat_synced_lfo() {
        let lfo =
            || LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0)));
        // Beat 1.0 of a 4-beat cycle = quarter phase = sine peak.
        let out = render_at(lfo(), stopped_at(1.0), 1);
        assert!((out[0] - 1.0).abs() < 0.01, "Expected 1.0, got {}", out[0]);
        let out = render_at(lfo(), stopped_at(2.0), 1);
        assert!((out[0] - 0.0).abs() < 0.01, "Expected 0.0, got {}", out[0]);
    }

    /// While the transport rolls, each frame is evaluated at its own beat,
    /// not the block's first: a sine over one 4-beat cycle, frame by frame,
    /// across blocks.
    #[test]
    fn a_rolling_transport_is_read_frame_by_frame() {
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        // 120 BPM at 48 kHz: 24 000 frames a beat, 96 000 a cycle.
        let out = render_at(
            LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0))),
            rolling,
            3_000,
        );
        for (i, &v) in out.iter().enumerate() {
            let beat = i as f64 / 24_000.0;
            let want = (core::f64::consts::TAU * beat / 4.0).sin() as f32;
            assert!((v - want).abs() < 1e-3, "frame {i}: {v} vs {want}");
        }
    }

    /// The beat is `f64` in the block's `Env`, so a phase far into a session
    /// keeps its sub-beat detail.
    #[test]
    fn beat_synced_lfo_keeps_sub_beat_precision_at_high_beats() {
        let lfo =
            || LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0)));
        // Same fractional offset (0.5 beat) at beat 0 and at beat 20000.
        let at_edge = render_at(lfo(), stopped_at(0.5), 1)[0];
        let past_edge = render_at(lfo(), stopped_at(20_000.5), 1)[0];

        // 20000 is a multiple of the 4-beat cycle, so both are the same phase.
        assert!(
            (at_edge - past_edge).abs() < 0.01,
            "sub-beat precision lost at high beat count: {at_edge} vs {past_edge}"
        );
    }

    #[test]
    fn test_depth_control() {
        let lfo = LfoNode::from(LfoConfig::new(LfoShape::Square).depth(Bipolar::of(0.5)));
        let out = render(lfo, 48_000.0, 1);
        assert!((out[0] - 0.5).abs() < 0.01);
    }

    /// Negative depth inverts the wave rather than silencing it.
    ///
    /// The depth is signed: `LfoConfig::depth(-1.0)` stores `-1.0` and phase-flips the
    /// shape. Clamping to `0.0..=1.0` instead would store `0.0` and take the
    /// LFO flat, which is silent in both senses.
    #[test]
    fn negative_depth_inverts_instead_of_silencing() {
        let first = |depth: f64| {
            let lfo = LfoNode::from(LfoConfig::new(LfoShape::Square).depth(Bipolar::of(depth)));
            render(lfo, 48_000.0, 1)[0]
        };
        let (a, b) = (first(1.0), first(-1.0));

        assert!(a.abs() > 0.01, "the reference sample must be audible");
        assert!((b + a).abs() < 1e-6, "expected {b} to invert to {}", -a);
    }

    #[test]
    fn test_phase_offset() {
        let lfo =
            LfoNode::from(LfoConfig::new(LfoShape::Sine).phase_offset(PhaseIncrement::new(0.25)));
        let out = render(lfo, 100.0, 1);
        assert!((out[0] - 1.0).abs() < 0.1, "Expected ~1.0, got {}", out[0]);
    }

    #[test]
    fn test_lfo_reset() {
        let mut d = Direct::new(
            LfoNode::from(LfoConfig::new(LfoShape::Sine)),
            SampleRate::of(100.0),
            50,
        );
        d.block();
        Node::<f32>::reset(&mut *d.node);
        d.block();
        assert!(
            d.output().plane(0).samples()[0].abs() < 0.1,
            "After reset, LFO should start near zero"
        );
    }

    /// The unit roundoff of `f32`, `2^-24`: the bound on the relative error of one rounding,
    /// so on the absolute error of a value in `[-1, 1]`.
    const F32_ROUNDOFF: f64 = 5.960_464_477_539_063e-8;

    fn random(seed: u64) -> Waveform {
        Waveform::Shape {
            shape: LfoShape::Random,
            seed,
        }
    }

    /// **Seek.** A beat-synced random LFO's value at a beat is the wave's at
    /// that beat's cycle, whether the transport rolled there from zero or
    /// was placed there: the step is addressed by the cycle index, not
    /// counted from wraps the node happened to see.
    #[test]
    fn a_random_value_is_the_same_after_a_seek() {
        let seed = 11;
        let lfo = || {
            LfoNode::from(
                LfoConfig::new(LfoShape::Random)
                    .seed(seed)
                    .beat_sync(BeatsPerCycle::of(1.0)),
            )
        };
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        // 7.5 beats at 120 BPM and 48 kHz.
        let rolled = render_at(lfo(), rolling, 180_001);
        let want = random(seed).raw_at(7.5, false);
        assert_eq!(rolled[180_000], want as f32);
        for beat in [7.5, 7.25, 3.5, 1_000.5] {
            let sought = render_at(lfo(), stopped_at(beat), 1)[0];
            assert_eq!(
                sought,
                random(seed).raw_at(beat, false) as f32,
                "beat {beat}"
            );
        }
        assert_eq!(render_at(lfo(), stopped_at(7.25), 1)[0], want as f32);
    }

    /// **Fork.** A free-running fork starts at cycle zero and replays the
    /// wave's values from there, as the original did: both read the value
    /// at each frame's cycle, `i / 256` at 187.5 Hz and 48 kHz (an exact
    /// binary step, so the position is exact).
    #[test]
    fn a_random_value_is_the_same_in_a_fork() {
        let rate = SampleRate::SR_48K;
        let mut d = Direct::new(
            LfoNode::from(
                LfoConfig::new(LfoShape::Random)
                    .seed(5)
                    .rate(Hz::new(187.5)),
            ),
            rate,
            4096,
        );
        let check = |out: &[f32], who: &str| {
            for (i, &v) in out.iter().enumerate() {
                assert_eq!(
                    v,
                    random(5).raw_at(i as f64 / 256.0, false) as f32,
                    "{who} frame {i}"
                );
            }
        };
        d.block();
        check(d.output().plane(0).samples(), "original");
        let mut f = Direct::new(ParamNode::<f32>::fork_fresh(&*d.node), rate, 4096);
        f.block();
        check(f.output().plane(0).samples(), "fork");
    }

    /// **Step boundaries.** The step changes exactly where the rate puts a
    /// cycle boundary, and nowhere else: every 256 frames free-running at
    /// 187.5 Hz, and every 12 000 frames beat-synced at half a beat per cycle
    /// (120 BPM, 48 kHz), each step the wave's value for its index. The
    /// smooth wave reaches each step at its boundary.
    #[test]
    fn random_steps_fall_where_the_rate_says() {
        let w = random(0);
        let free = render(
            LfoNode::from(LfoConfig::new(LfoShape::Random).rate(Hz::new(187.5))),
            48_000.0,
            2048,
        );
        for (i, &v) in free.iter().enumerate() {
            assert_eq!(
                v,
                w.raw_at((i / 256) as f64, false) as f32,
                "free frame {i}"
            );
        }
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        let synced = render_at(
            LfoNode::from(LfoConfig::new(LfoShape::Random).beat_sync(BeatsPerCycle::of(0.5))),
            rolling,
            60_000,
        );
        for (i, &v) in synced.iter().enumerate() {
            assert_eq!(
                v,
                w.raw_at((i / 12_000) as f64, false) as f32,
                "synced frame {i}"
            );
        }
        // Consecutive steps differ here, so each boundary is visible.
        for k in 1..5 {
            assert_ne!(synced[k * 12_000 - 1], synced[k * 12_000], "boundary {k}");
        }
        let smooth = render(
            LfoNode::from(LfoConfig::new(LfoShape::RandomSmooth).rate(Hz::new(187.5))),
            48_000.0,
            2048,
        );
        for k in 1..8 {
            assert!(
                (f64::from(smooth[k * 256]) - w.raw_at(k as f64 - 1.0, false)).abs() < F32_ROUNDOFF
            );
        }
    }

    /// The node built from a control-rate [`tutti_modulation::Lfo`] evaluates the
    /// same wave: seed, phase and rate carried over. Beat-synced, on the same
    /// clock (the beat); free-running, at the same rate on the node's
    /// own clock, which counts from its insert (frame 0 here).
    #[test]
    fn a_node_from_a_control_lfo_reads_its_wave() {
        let lfo = tutti_modulation::Lfo::new(LfoShape::RandomSmooth)
            .seed(3)
            .rate(BeatsPerCycle::of(2.0))
            .phase(PhaseIncrement::new(0.25));
        let wave = Waveform::Shape {
            shape: LfoShape::RandomSmooth,
            seed: 3,
        };
        for beat in [0.0, 1.5, 9.0, 33.75] {
            let got = render_at(LfoNode::from(LfoConfig::from(lfo)), stopped_at(beat), 1)[0];
            let want = wave.raw_at(beat / 2.0 + 0.25, false);
            assert!(
                (f64::from(got) - want).abs() < F32_ROUNDOFF,
                "beat {beat}: {got} vs {want}"
            );
        }
        // 187.5 Hz at 48 kHz: 256 frames a cycle.
        let free = render(
            LfoNode::from(LfoConfig::from(lfo.rate(Hz::new(187.5)))),
            48_000.0,
            2048,
        );
        for i in [0, 100, 256, 777, 2047] {
            let want = wave.raw_at(i as f64 / 256.0 + 0.25, false);
            let got = f64::from(free[i]);
            assert!(
                (got - want).abs() < F32_ROUNDOFF,
                "frame {i}: {got} vs {want}"
            );
        }
    }

    #[test]
    fn test_random_produces_different_values() {
        let values = render(LfoNode::from(LfoConfig::new(LfoShape::Random)), 100.0, 500);

        let unique: std::collections::HashSet<u32> =
            values.iter().map(|v| (v * 1000.0) as u32).collect();

        assert!(
            unique.len() > 1,
            "Random LFO should produce different values, got {}",
            unique.len()
        );
    }

    #[test]
    fn a_reverse_rate_walks_the_cycle_backwards_across_the_start() {
        // -1 Hz at 100 Hz: frame n is at -n / 100 cycles, whose ramp value is
        // `2 frac(-n / 100) - 1`; frame 0 is on the start, the ramp's -1.
        let out = render(
            LfoNode::from(LfoConfig::new(LfoShape::Sawtooth).rate(Hz::new(-1.0))),
            100.0,
            250,
        );
        for (n, &got) in out.iter().enumerate().filter(|(n, _)| n % 100 != 0) {
            let want = 2.0 * (1.0 - (n % 100) as f64 / 100.0) - 1.0;
            assert!((f64::from(got) - want).abs() < 1e-6, "frame {n}: {got}");
        }
    }

    /// The random wave `Random` of seed 0 at `cycles`, the closed form a free-running
    /// `LfoNode` is tested against.
    fn random_at(cycles: f64) -> f32 {
        Waveform::Shape {
            shape: LfoShape::Random,
            seed: 0,
        }
        .raw_at(cycles, false) as f32
    }

    /// At 100 Hz the clock's step is `rate / 100` cycles; frames whose position is not
    /// within 0.05 of a cycle boundary have an index no rounding can move.
    fn assert_random_follows(rate: f64, step: f64) {
        let out = render(
            LfoNode::from(LfoConfig::new(LfoShape::Random).rate(Hz::new(rate))),
            100.0,
            200,
        );
        let mut checked = 0;
        for (n, &got) in out.iter().enumerate() {
            let pos = n as f64 * step;
            if (pos - pos.round()).abs() < 0.05 {
                continue;
            }
            assert_eq!(got, random_at(pos), "rate {rate}, frame {n}");
            checked += 1;
        }
        assert!(checked > 80);
    }

    #[test]
    fn a_random_lfo_at_a_high_rate_below_nyquist_carries_forward_once_a_cycle() {
        // 40 Hz at 100 Hz: 0.4 cycles a frame.
        assert_random_follows(40.0, 0.4);
    }

    #[test]
    fn a_random_lfo_at_nyquist_and_past_it_runs_forward_at_nyquist() {
        // 50 Hz is exactly half a cycle a frame and 75 Hz is 1.5 times that; both clamp
        // to just under half a cycle, so the cycle index steps every second frame.
        assert_random_follows(50.0, 0.5);
        assert_random_follows(75.0, 0.5);
        assert_random_follows(-75.0, -0.5);
    }

    #[test]
    fn a_random_lfo_holds_one_value_a_cycle_and_steps_on_the_next() {
        // 1 Hz at 100 Hz: cycles are 100 frames; frames 10..=90 of each are one cycle.
        let out = render(LfoNode::from(LfoConfig::new(LfoShape::Random)), 100.0, 300);
        for c in 0..3 {
            let held = out[100 * c + 10];
            assert!(out[100 * c + 10..=100 * c + 90].iter().all(|&v| v == held));
        }
        assert!(out[10] != out[110] && out[110] != out[210]);
    }

    /// In an `f64` graph a free-running sine at a third of the rate is `sin(2 pi k / 3)`. The
    /// wave and the depth are `f64` (`Waveform::raw_at`, `Bipolar`), so every frame holds the
    /// closed form to `F64_TOLERANCE`, and a frame is not what an `f32` narrowing leaves (a
    /// non-dyadic value, which `f32` cannot hold to 1e-9).
    #[test]
    fn in_an_f64_graph_the_lfo_writes_its_wave_unnarrowed() {
        use crate::test_support::{F64_TOLERANCE, RATE, render_f64};
        let node = LfoNode::from(LfoConfig::new(LfoShape::Sine).rate(Hz::new(RATE.get() / 3.0)));
        let out = render_f64(node, RATE, &[], 64);
        let mut beyond_f32 = false;
        for (k, &got) in out[0].iter().enumerate() {
            let want = (core::f64::consts::TAU * k as f64 / 3.0).sin();
            assert!(
                (got - want).abs() <= F64_TOLERANCE,
                "frame {k}: {got} vs {want}"
            );
            beyond_f32 |= (got - f64::from(got as f32)).abs() > 1e-9;
        }
        assert!(beyond_f32, "no frame is finer than an f32");
    }
}
