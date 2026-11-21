//! Phaser: a swept all-pass chain, of any width.
//!
//! [`PhaserChain`] is the algorithm (the all-pass chain and its feedback); this file holds
//! the sweep (the LFO and the solve of the complements), the cells and how the graph reaches
//! them. The one modulation effect here with no delay line: it notches by phase
//! cancellation, which is why its notches are fewer and unevenly spaced compared with a
//! flanger's.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, GAIN_RAMP, Lerp, MAX_PHASER_CHANNELS, MIN_RATE, PhaserChain,
    PhaserControls, Ramp, Stages, Stagger, StaggerError, Steps, SweepRange, allpass_complement,
    sweep_hz,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_units::{Depth, Feedback, Hz, PhaseIncrement, SampleRate, Tail};

use super::sweep_clock::{SEGMENT, SweepClock};
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// Phaser of any width: `N` audio inputs, `N` outputs.
///
/// A [`KernelNode`] over [`PhaserKernel`]: a chain of all-pass stages ([`PhaserChain`]) whose
/// centre frequency an internal LFO sweeps across a [`SweepRange`]. Each stage passes every
/// frequency at unity and shifts only phase; blending that against the dry signal (the
/// graph's mix, `Editor::set_mix`: the node outputs the wet signal alone) turns the shift
/// into cancellation notches, and sweeping the centre moves them. An all-pass chain alone
/// barely changes the magnitude, so a phaser wants a mix below one, deepest at one half.
///
/// **This is not a delay effect.** Unlike chorus and flanger it holds no delay line, so
/// its notches are unevenly spaced and fewer, the reason a phaser sounds hollower and less
/// metallic than a flanger. The [`Stages`] set how many notches there are (a pair each);
/// feedback deepens them.
///
/// One LFO serves every channel; each channel keeps its own all-pass state, so the channels
/// phase identically without bleeding into each other. Every channel sweeps in step by
/// default, which widens nothing by itself: a [`Stagger`] (`[0.0, 0.25]` cycles on a stereo
/// phaser sweeps the sides a quarter cycle apart) is what widens it.
///
/// Made from a [`PhaserConfig`] (`PhaserNode::try_from`): any width on a checked
/// [`BusTopology`] (an `n`-channel one on the engine's order for that width is
/// `BusTopology::default_for(n)?`), `stages` stages sweeping 200 to 4000 Hz with no stagger unless
/// the config gives its own [`SweepRange`] and [`Stagger`].
///
/// # The controls
///
/// The ports are in this order: **rate** ([`Rate`](tutti_units::UnitParam::Rate), [`Hz`]),
/// **depth** ([`Depth`](tutti_units::UnitParam::Depth), unitless) and [`Feedback`]. All
/// three are read on the stream's 16-frame grid, so what is heard does not depend on how the
/// stream is cut into blocks; a driven port is read at the grid's solve frame.
///
/// - The **rate** is how fast the notches sweep: phaser rates are slow, typically 0.1 to
///   2 Hz. **Floored at 0.01 Hz, a control signal's value and a cell's alike.** It lands whole, and the
///   LFO's phase is continuous across it.
/// - The **depth** is a fraction of the sweep range, `0..=1` (a chorus's is seconds of
///   delay): 1 sweeps the whole range, 0 parks the notches at its bottom. A cell that moves
///   ramps over [`GAIN_RAMP`].
/// - The **feedback** is how much of the chain's output recirculates, clamped into
///   `0..=`[`Feedback::MAX_STABLE`]. A cell that moves ramps over
///   [`GAIN_RAMP`].
///
/// The sweep's top is held at 0.90 of Nyquist at the graph's rate, and the authored top is
/// kept, so a node prepared at a higher rate restores it.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)), `N` in and `N` out, with no latency.
/// Inserted, its controls are a [`ParamSet`](tutti_graph::ParamSet) over those three; a fork
/// starts from the values last set through it, with the LFO back at the start. The graph
/// prepares it at the device rate before its first block, and **a re-prepare restarts the
/// sweep**. Its tail is [`Tail::Unknown`] (the feedback
/// recirculates), so it is never skipped.
///
/// `F` is the float of the graph it runs in, and of the chain and the ramps.
pub type PhaserNode<F> = KernelNode<PhaserKernel<F>>;

/// What a [`PhaserNode`] is made from: its bus, the all-pass stages per channel, the band the
/// sweep crosses, the [`Stagger`] of its channels and the controls it starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaserConfig {
    input: BusTopology,
    stages: Stages,
    range: SweepRange,
    stagger: Stagger,
    controls: PhaserControls,
}

impl PhaserConfig {
    /// A phaser on the bus `input`, as wide as it, with `stages` all-passes per channel
    /// sweeping [`SweepRange::DEFAULT`], its channels in step, at
    /// [`PhaserControls::DEFAULT`].
    #[must_use]
    pub const fn new(input: BusTopology, stages: Stages) -> Self {
        Self {
            input,
            stages,
            range: SweepRange::DEFAULT,
            stagger: Stagger::NONE,
            controls: PhaserControls::DEFAULT,
        }
    }

    /// The band the all-pass centre sweeps across.
    #[must_use]
    pub const fn range(mut self, range: SweepRange) -> Self {
        self.range = range;
        self
    }

    /// Offsets the channels' sweeps by `stagger`: a step, or one offset for each channel of
    /// the bus.
    #[must_use]
    pub const fn stagger(mut self, stagger: Stagger) -> Self {
        self.stagger = stagger;
        self
    }

    /// The controls the cells start at.
    #[must_use]
    pub const fn controls(mut self, controls: PhaserControls) -> Self {
        self.controls = controls;
        self
    }
}

/// A phaser built to `config`. Allocates the chain.
///
/// A `TryFrom` and not a `From`: a stagger that names an offset for each channel must name
/// the bus's, which no type of the config can hold on its own. A step fits every width.
impl<F: Float + Lerp> TryFrom<PhaserConfig> for PhaserNode<F> {
    type Error = StaggerError;

    /// # Errors
    ///
    /// [`StaggerError::Length`] when the stagger names an offset for each channel and not for
    /// the bus's.
    fn try_from(config: PhaserConfig) -> Result<Self, StaggerError> {
        Ok(Self::with_kernel(
            PhaserKernel::new(config.input, config.stages, config.range, config.stagger)?,
            PhaserSet,
            config.controls,
        ))
    }
}

/// The phaser's controls, in port order: rate, depth, feedback. Each lands whole on the
/// stream grid (the kernel's own ramps are the depth's and the feedback's: the reader must
/// not ramp them across the block as well).
#[derive(Clone, Copy, Debug)]
pub struct PhaserSet;

control_set! {
    PhaserSet(_s): PhaserControls = PhaserControls::DEFAULT;
    live: (all);
    // How fast the notches sweep: floored at 0.01 Hz, a control signal's value and a cell's alike.
    Rate => Hz = rate { whole },
    // A fraction of the sweep range.
    Depth => Depth = depth { whole },
    Feedback => Feedback = feedback { whole };

    fn defaults(&self) -> PhaserControls {
        PhaserControls::DEFAULT
    }
}

/// The phaser as a [`Kernel`]: a [`PhaserChain`], the LFO that sweeps it and the solve of its
/// complements.
///
/// **The LFO is state on the stream's clock.** Its phase is a `Phase` walked by the
/// rate's increment, counted from the last reset (and so from every `prepare`: **a
/// re-prepare restarts the sweep**). It is not anchored to the transport. The chain's
/// coefficient is solved at the **corners** of a segment, every [`COEFF_INTERVAL`] frames
/// of the stream (a `tan` per corner and channel): the segment's start and its end, and each
/// channel's coefficient ramps between them, so the cut of the blocks does not move it: the
/// render is the same whatever the blocks. The chord of the sweep over a segment misses it by
/// a few millionths at the sweep rates a phaser has (3.5e-6 for 2 Hz), and is exact at a
/// depth of zero, where every corner is equal. The LFO steps before the frame it sounds in:
/// frame `i` of a segment sounds the sweep `i + 1` steps on from the segment's start.
/// A rate that moves lands whole at the next grid point and the phase is continuous across it.
///
/// A depth and a feedback that move ramp over [`GAIN_RAMP`]: a depth move shifts the
/// notches, and the feedback is a loop gain. The depth is solved into a segment's end corner
/// with the depth the ramp has there.
///
/// The chain has **no latency**. Its tail is [`Tail::Unknown`]: the closed loop's ring-out has
/// no closed form a tail could be cut at.
#[derive(Clone, Debug)]
pub struct PhaserKernel<F: Float> {
    input: BusTopology,
    chain: PhaserChain<F>,
    range: SweepRange,
    /// The sweep's bottom and top in [`Hz`]: the top is the authored one, held at 0.90 of
    /// Nyquist at the prepared rate.
    min: Hz,
    max: Hz,
    rate: SampleRate,
    /// The ramp length of the depth and the feedback: [`GAIN_RAMP`] at the prepared rate.
    steps: u32,
    /// The coefficient of each channel across the segment in flight, from its start corner.
    complements: Box<[Ramp<F>]>,
    depth: Ramp<F>,
    feedback: Ramp<F>,
    /// The LFO and the segments.
    clock: SweepClock,
}

impl<F: Float + Lerp> PhaserKernel<F> {
    /// A phaser on the bus `input`, `stages` all-passes per channel sweeping `range`, its
    /// channels' sweeps offset by `stagger`. Allocates the chain.
    ///
    /// # Errors
    ///
    /// [`StaggerError::Length`] when `stagger` names an offset for each channel
    /// ([`Stagger::try_from`] a slice) and not for the `input`'s.
    fn new(
        input: BusTopology,
        stages: Stages,
        range: SweepRange,
        stagger: Stagger,
    ) -> Result<Self, StaggerError> {
        let width = usize::from(input.layout().count());
        match stagger.channels() {
            Some(got) if got != width => return Err(StaggerError::Length { width, got }),
            _ => {}
        }
        let mut offsets = [PhaseIncrement::new(0.0); MAX_PHASER_CHANNELS];
        let layout = input.layout();
        for (c, o) in (0..width).filter_map(|c| layout.index(c)).zip(&mut offsets) {
            *o = stagger.offset(c);
        }
        let d = PhaserControls::DEFAULT;
        Ok(Self {
            input,
            chain: PhaserChain::new(stages, input.layout()),
            range,
            min: range.min(),
            max: range.max(),
            rate: SampleRate::SR_48K,
            steps: 0,
            complements: vec![Ramp::new(F::ONE); width].into_boxed_slice(),
            depth: Ramp::new(F::from_unit(d.depth)),
            feedback: Ramp::new(F::from_unit(d.feedback)),
            clock: SweepClock::new(offsets),
        })
    }

    /// Solves the next segment: its start corner is the last one's end (or the LFO's start
    /// after a reset), its end corner the sweep [`COEFF_INTERVAL`] frames on at the LFO's
    /// step and the depth the ramp has there.
    fn solve(&mut self) {
        let depth_start = self.depth.now();
        let depth_end = self
            .depth
            .at(self.depth.steps_taken().saturating_add(SEGMENT));
        let (min, max, rate) = (self.min, self.max, self.rate);
        self.clock.solve(&mut self.complements, |lfo, end| {
            let depth = if end { depth_end } else { depth_start };
            let sweep = sweep_hz(min, max, lfo, Depth::clamped(depth.into()));
            allpass_complement(sweep, rate)
        });
    }
}

impl<F: Float + Lerp> Kernel for PhaserKernel<F> {
    type Float = F;
    type Set = PhaserSet;

    /// The corners solve on the stream's grid of 16 frames, like the filters and the chorus:
    /// a cell that moves inside a block reaches the render within 16 frames, wherever the
    /// blocks fall.
    const GRID: usize = COEFF_INTERVAL;

    /// The LFO moves every frame, so every grid point solves a segment.
    const CUT_ALWAYS: bool = true;

    fn shape(&self) -> Shape {
        let output = Bus::banded(self.input, BusRole::Main, 1).expect("one band is a bus");
        Shape::buses(
            Buses::main(self.input),
            Buses::NONE.with(output).expect("one bus"),
        )
        .with_cost(Cost::Moderate)
    }

    /// Prepares for `rate`: the sweep's top is held at 0.90 of Nyquist there (the authored
    /// top is kept, so a higher rate restores it), the ramps take [`GAIN_RAMP`] at it
    /// (the conversion saturates: a prepare must not panic), and the sweep restarts.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.min = self.range.min();
        self.max = self.range.max().min(rate.nyquist_scaled(0.90));
        self.steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        self.reset();
    }

    /// Back to silence with the sweep at its start: the chain empty, the ramps at rest on
    /// their targets, the next segment solved from the LFO's first phase and the next design
    /// landing at once, as a fresh node's first does.
    fn reset(&mut self) {
        self.chain.reset();
        self.depth.jump_to_target();
        self.feedback.jump_to_target();
        self.clock.reset();
        self.clock.unland();
    }

    /// Aims the depth and the feedback at `controls`, takes the rate's step, and at a
    /// segment's start solves the segment. The rate lands whole at that grid point and the
    /// LFO's phase is continuous across it; a retarget inside a segment is a control that
    /// moved, and waits for the next grid point. The first retarget after a prepare lands
    /// the depth and the feedback at once.
    fn retarget(&mut self, controls: &PhaserControls) {
        // A control reaches here finite (the cells' reader holds the last finite value); the
        // rate is floored and the feedback clamped.
        self.clock.set_inc(PhaseIncrement::per_sample(
            Hz::new(controls.rate.get().max(MIN_RATE.get())),
            self.rate,
        ));
        let feedback = Feedback::new_clamped(controls.feedback.get());
        let steps = Steps::Fixed(self.steps);
        self.depth.retarget_by(F::from_unit(controls.depth), steps);
        self.feedback.retarget_by(F::from_unit(feedback), steps);
        if self.clock.lands() {
            self.depth.jump_to_target();
            self.feedback.jump_to_target();
        }
        if self.clock.at_corner() {
            self.solve();
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let (input, output) = block.split();
        let frames = input.frames().get().min(output.frames().get());
        self.chain
            .process(&mut self.complements, &mut self.feedback, input, output);
        self.depth
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        self.clock.advance(frames);
        Ok(())
    }

    /// The closed loop's ring-out has no bound a tail could be cut at: unknown, so the
    /// graph never reads it as none and the node never parks.
    fn tail(&self) -> Tail {
        Tail::Unknown
    }
}

#[cfg(test)]
mod tests {
    use tutti_buffers::Planes;
    // The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
    // format, so their tolerances are `f32`'s.
    use super::*;
    use crate::test_support::drive_kernel;
    use tutti_buffers::ChannelLayout;
    use tutti_graph::contract::{assert_declared_tail, assert_param_fork, drive, prepared};
    use tutti_graph::{Node, ParamNode};
    use tutti_units::{
        Bipolar, Latency, Phase, PhaseIncrement, PhaseStep, Real, RunningPhase, Samples, Tail,
        UnitParam,
    };

    const RATE: SampleRate = SampleRate::SR_48K;

    fn bus(width: u16) -> BusTopology {
        BusTopology::default_for(ChannelLayout::from(width)).expect("a test width is one bus")
    }

    /// A fork starts from the values last set through the node's `ParamSet` and shares no
    /// cell with it, and the set has the three controls in the kernel's order.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node =
            PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::STEREO, Stages::of(4)))
                .unwrap();
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Rate, UnitParam::Depth, UnitParam::Feedback]
        );
        assert_param_fork(node);
    }

    /// The node declares rate, depth and feedback as its params in that order, each with its
    /// control as the base, as wide as it was built, in and out, with no latency (the sweep
    /// is the sound) and an unknown tail (the feedback recirculates).
    #[test]
    fn the_shape_declares_the_three_controls_no_latency_and_an_unknown_tail() {
        let node = prepared(
            PhaserNode::<f32>::try_from(PhaserConfig::new(bus(3), Stages::of(5))).unwrap(),
            RATE,
            64,
        );
        let shape = node.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Rate, UnitParam::Depth, UnitParam::Feedback][..]
        );
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.latency, Latency::new(Samples::new(0)));
        assert_eq!(shape.tail, Tail::Unknown);
        assert_eq!(node.param_base(0), Some(0.3), "the rate's base is its cell");
        assert_eq!(node.param_base(1), Some(0.5));
        assert_eq!(node.param_base(2), Some(0.5));
        assert_eq!(node.param_base(3), None, "no fourth param");
        let mut node = node;
        assert_declared_tail(&mut node, RATE);
    }

    /// A stagger that names an offset per channel must name the bus's.
    #[test]
    fn a_stagger_of_the_wrong_length_is_refused_at_construction() {
        let two =
            Stagger::try_from(&[PhaseIncrement::new(0.0), PhaseIncrement::new(0.5)][..]).unwrap();
        let new =
            |w| PhaserNode::<f32>::try_from(PhaserConfig::new(bus(w), Stages::of(4)).stagger(two));
        assert!(new(2).is_ok());
        assert_eq!(
            new(3).unwrap_err(),
            StaggerError::Length { width: 3, got: 2 }
        );
        // A step, and no stagger, fit any width.
        for width in [1, 2, 6, 16] {
            for any in [Stagger::of_step(PhaseIncrement::new(0.25)), Stagger::NONE] {
                let node = PhaserNode::<f32>::try_from(
                    PhaserConfig::new(bus(width), Stages::of(4)).stagger(any),
                );
                assert!(node.is_ok(), "width {width}");
            }
        }
    }

    /// The textbook phase of one stage tuned to `hz`, from `H(z) = (c + z^-1) / (1 + c z^-1)`
    /// with `c = (tan(pi hz / sr) - 1) / (tan(pi hz / sr) + 1)`, at `at` Hz.
    fn stage_phase(hz: f64, at: f64) -> f64 {
        let t = (std::f64::consts::PI * hz / RATE.get()).tan();
        let c = (t - 1.0) / (t + 1.0);
        let w = std::f64::consts::TAU * at / RATE.get();
        let (zr, zi) = (w.cos(), -w.sin());
        let (nr, ni) = (c + zr, zi);
        let (dr, di) = (1.0 + c * zr, c * zi);
        (ni * dr - nr * di).atan2(nr * dr + ni * di)
    }

    /// The rate is **floored at 0.01 Hz, a control signal's value and a cell's alike**: a rate of -3 Hz,
    /// written to the cell or fed on the port, is the 0.01 Hz sweep, which after 0.1 s has
    /// moved the LFO a thousandth of a cycle, so the centre of a 4-stage phaser at full depth
    /// over 200 to 4000 Hz is `200 + 3800 (1 + sin(2 pi 0.001)) / 2`. A 1 kHz sine comes out
    /// with the phase of four stages tuned there (a quasi-static prediction from the textbook
    /// stage, since the coefficient has barely moved). Running backwards at 3 Hz would have
    /// the centre near its trough, 0.3 of a cycle behind, and a different phase.
    #[test]
    fn a_rate_below_the_floor_is_the_floor_in_a_cell_and_on_a_signal() {
        let n = 4_800;
        let centre = 200.0 + 3_800.0 * 0.5 * (1.0 + (std::f64::consts::TAU * 0.001).sin());
        let wanted = 4.0 * stage_phase(centre, 1_000.0);
        let backwards = 200.0 + 3_800.0 * 0.5 * (1.0 + (-std::f64::consts::TAU * 0.3).sin());
        assert!(
            (4.0 * stage_phase(backwards, 1_000.0) - wanted).abs() > 0.1,
            "a backwards sweep would be heard"
        );
        let controls = PhaserControls {
            depth: Depth::of(1.0),
            feedback: Feedback::NONE,
            ..PhaserControls::DEFAULT
        };
        let x: Vec<f32> = (0..n + 64)
            .map(|i| (std::f64::consts::TAU * 1_000.0 * i as f64 / RATE.get()).sin() as f32)
            .collect();
        let render = |cell: Option<f32>, signal: Option<f32>| {
            let node = PhaserNode::<f32>::try_from(
                PhaserConfig::new(BusTopology::MONO, Stages::of(4)).controls(controls),
            )
            .unwrap();
            if let Some(v) = cell {
                assert!(node.param_set().set(UnitParam::Rate, f64::from(v)));
            }
            let mut node = prepared(node, RATE, 64);
            let mut y = Vec::new();
            for chunk in x.chunks(64) {
                let rate = signal.map(|v| vec![v; chunk.len()]);
                let params = [rate.as_deref(), None, None];
                y.extend(drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &params).remove(0));
            }
            y
        };
        for (what, y) in [
            ("cell", render(Some(-3.0), None)),
            ("signal", render(None, Some(-3.0))),
        ] {
            let w = std::f64::consts::TAU * 1_000.0 / RATE.get();
            let worst = (n - 256..n)
                .map(|i| (f64::from(y[i]) - (w * i as f64 + wanted).sin()).abs())
                .fold(0.0, f64::max);
            assert!(worst < 5e-3, "a negative rate on a {what}: {worst}");
        }
    }

    /// A depth control signal past 1 sweeps the whole range and no further: at 0.01 Hz after 0.1 s the
    /// LFO is a thousandth of a cycle on, so a 1 kHz sine through four stages comes out with
    /// the phase of four stages tuned to `200 + 3800 (1 + sin(2 pi 0.001)) / 2` Hz, the
    /// sweep at depth exactly 1 (a control signal at 8 unclamped would put the centre at 15 kHz and more).
    #[test]
    fn a_depth_signal_past_one_sweeps_the_whole_range_and_no_further() {
        let n = 4_800;
        let centre = 200.0 + 3_800.0 * 0.5 * (1.0 + (std::f64::consts::TAU * 0.001).sin());
        let wanted = 4.0 * stage_phase(centre, 1_000.0);
        let unclamped = 200.0 + 3_800.0 * 0.5 * 8.0;
        assert!((4.0 * stage_phase(unclamped, 1_000.0) - wanted).abs() > 0.1);
        let controls = PhaserControls {
            rate: Hz::new(0.01),
            feedback: Feedback::NONE,
            ..PhaserControls::DEFAULT
        };
        let mut node = prepared(
            PhaserNode::<f32>::try_from(
                PhaserConfig::new(BusTopology::MONO, Stages::of(4)).controls(controls),
            )
            .unwrap(),
            RATE,
            64,
        );
        let x: Vec<f32> = (0..n + 64)
            .map(|i| (std::f64::consts::TAU * 1_000.0 * i as f64 / RATE.get()).sin() as f32)
            .collect();
        let mut y = Vec::new();
        for chunk in x.chunks(64) {
            let signal = vec![8.0f32; chunk.len()];
            let params = [None, Some(&signal[..]), None];
            y.extend(drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &params).remove(0));
        }
        let w = std::f64::consts::TAU * 1_000.0 / RATE.get();
        let worst = (n - 256..n)
            .map(|i| (f64::from(y[i]) - (w * i as f64 + wanted).sin()).abs())
            .fold(0.0, f64::max);
        assert!(worst < 5e-3, "depth driven to 8: {worst}");
    }

    /// A control the graph drives is sampled on the stream's 16-frame grid, not at the block's
    /// last frame, so what is heard does not depend on how the stream is cut into blocks
    /// (the node against itself under cuts, not a reference render); and the control signal is heard,
    /// since the cell it overrides is a different depth.
    #[test]
    fn a_driven_slow_control_is_sampled_on_the_grid_whatever_the_blocks() {
        let len = 4_096;
        let signal: Vec<f32> = (0..len)
            .map(|n| 0.1 + 0.9 * n as f32 / len as f32)
            .collect();
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |block: usize, driven: bool| -> Vec<f32> {
            let mut node = prepared(
                PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::MONO, Stages::of(6)))
                    .unwrap(),
                RATE,
                1_024,
            );
            let mut out = Vec::new();
            for (n, chunk) in x.chunks(block).enumerate() {
                let at = n * block;
                let l = &signal[at..at + chunk.len()];
                let params = [None, driven.then_some(l), None];
                out.extend(
                    drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &params).remove(0),
                );
            }
            out
        };
        let whole = render(1_024, true);
        assert_ne!(whole, render(1_024, false), "the signal is heard");
        for block in [1, 16, 64, 128] {
            assert_eq!(render(block, true), whole, "blocks of {block}");
        }
    }

    /// A cell that moves lands on the stream's grid, whatever the blocks: the rate, the depth
    /// and the feedback written between two blocks at frame 1024 (a multiple of every block
    /// below) give the same render in blocks of 1, 16, 64, 128 and 1024 (the node against
    /// itself under cuts), because the reader takes each whole at the grid and the
    /// kernel's ramps are on the stream's clock, not a block's. And the move is heard.
    #[test]
    fn a_moved_cell_lands_on_the_grid_whatever_the_blocks() {
        let len = 4_096;
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |block: usize, moved: bool| -> Vec<f32> {
            let node =
                PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::MONO, Stages::of(6)))
                    .unwrap();
            let set = node.param_set();
            let mut node = prepared(node, RATE, 1_024);
            let mut out = Vec::new();
            for (n, chunk) in x.chunks(block).enumerate() {
                if moved && n * block == 1_024 {
                    assert!(set.set(UnitParam::Rate, 8.0));
                    assert!(set.set(UnitParam::Depth, 0.2));
                    assert!(set.set(UnitParam::Feedback, 0.1));
                }
                out.extend(drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &[]).remove(0));
            }
            out
        };
        let whole = render(1_024, true);
        assert_ne!(whole, render(1_024, false), "the move is heard");
        for block in [1, 16, 64, 128] {
            assert_eq!(render(block, true), whole, "blocks of {block}");
        }
    }

    /// Feedback recirculates: after an impulse, a 6-stage phaser at 0.9 feedback is still
    /// ringing 500 frames on, where with none the all-pass chain's response has decayed away
    /// (an all-pass section's pole is its coefficient, `|c| < 1`, and the chain's impulse
    /// response is under 1e-3 well before that at these centres).
    #[test]
    fn feedback_keeps_the_chain_ringing() {
        let ring = |fb: f64| {
            let node = PhaserNode::<f32>::try_from(
                PhaserConfig::new(BusTopology::MONO, Stages::of(6)).controls(PhaserControls {
                    feedback: Feedback::new(fb),
                    ..PhaserControls::DEFAULT
                }),
            )
            .unwrap();
            let mut node = prepared(node, SampleRate::SR_44K1, 1_024);
            let mut x = [0.0f32; 1_024];
            x[0] = 1.0;
            let y = drive(
                &mut node,
                SampleRate::SR_44K1,
                Planes::new(&[&x]).unwrap(),
                &[],
            )
            .remove(0);
            y[500..].iter().fold(0.0f32, |m, s| m.max(s.abs()))
        };
        assert!(ring(0.9) > 1e-3, "feedback rings: {}", ring(0.9));
        assert!(ring(0.0) < 1e-3, "no feedback does not: {}", ring(0.0));
    }

    // ---- the kernel: segments, corners and the sweep ----------------------------------

    fn controls(rate: f64, depth: f64, feedback: f64) -> PhaserControls {
        PhaserControls {
            rate: Hz::new(rate),
            depth: Depth::of(depth),
            feedback: Feedback::new(feedback),
        }
    }

    /// A kernel on `width` channels, prepared at [`RATE`].
    fn kernel(width: u16, stages: usize, range: SweepRange, stagger: Stagger) -> PhaserKernel<f32> {
        let mut k = PhaserKernel::<f32>::new(bus(width), Stages::of(stages), range, stagger)
            .expect("a stagger that fits");
        k.prepare(RATE, MaxBlock::of(Samples::new(64)));
        k
    }

    /// The complement `1 + c` of the coefficient the sweep's definition gives for a channel
    /// offset by `offset` cycles, `steps` steps of the LFO on (a `Phase` walked one frame at a
    /// time), at depth `depth`, on `range` at `rate` Hz.
    fn exact_complement(
        range: SweepRange,
        rate: f64,
        depth: f64,
        offset: f64,
        steps: usize,
    ) -> f32 {
        let inc = PhaseIncrement::per_sample(Hz::new(rate), RATE);
        let mut p = Phase::START;
        for _ in 0..steps {
            p = p.advance(inc);
        }
        let lfo = Bipolar::clamped((std::f64::consts::TAU * (p.get() + offset)).sin());
        let hz = sweep_hz(range.min(), range.max(), lfo, Depth::clamped(depth));
        allpass_complement(hz, RATE)
    }

    fn silence(width: usize, frames: usize) -> Vec<Vec<f32>> {
        vec![vec![0.0; frames]; width]
    }

    /// What a segment's corners are: the coefficient at the LFO's phase at the segment's
    /// start and sixteen frames on, the sweep of the definition at depth 1, the second one
    /// a little past the first.
    #[test]
    fn the_corners_of_a_segment_are_the_sweep_at_the_lfo_phase() {
        let band = SweepRange::of(Hz::new(300.0), Hz::new(3_000.0));
        let mut k = kernel(1, 4, band, Stagger::NONE);
        k.retarget(&controls(5.0, 1.0, 0.5));
        let start = exact_complement(band, 5.0, 1.0, 0.0, 0);
        let end = exact_complement(band, 5.0, 1.0, 0.0, 16);
        assert!((k.complements[0].now() - start).abs() < 1e-6, "the start");
        assert!((k.complements[0].target() - end).abs() < 1e-6, "the end");
        assert_ne!(start, end, "the sweep moves");
    }

    /// Channel `c`'s sweep is `c` times the step ahead (wrapped), and a stagger of a whole
    /// cycle is none: each channel's coefficient at the first corner is the sweep at the
    /// sine of its own offset.
    #[test]
    fn each_channel_is_staggered_by_its_step_and_a_full_cycle_is_none() {
        let band = SweepRange::of(Hz::new(300.0), Hz::new(3_000.0));
        for (stagger, offsets) in [
            (
                Stagger::of_step(PhaseIncrement::new(0.25)),
                [0.0f64, 0.25, 0.5],
            ),
            (Stagger::of_step(PhaseIncrement::new(1.0)), [0.0, 0.0, 0.0]),
            (
                Stagger::of_step(PhaseIncrement::new(1.25)),
                [0.0, 0.25, 0.5],
            ),
        ] {
            let mut k = kernel(3, 4, band, stagger);
            k.retarget(&controls(1.0, 1.0, 0.5));
            for (ch, offset) in offsets.into_iter().enumerate() {
                let want = exact_complement(band, 1.0, 1.0, offset, 0);
                let got = k.complements[ch].now();
                assert!(
                    (got - want).abs() < 1e-5,
                    "channel {ch}: {got} against {want}"
                );
            }
        }
    }

    /// Between two corners the complement is the chord, read a step ahead: frame `16 k + j`
    /// sounds `prev + (next - prev) (j + 1) / 16`, and the coefficient is that less 1. An
    /// impulse at frame `n` of a fresh chain
    /// with no feedback comes out as that frame's coefficient once a stage (`out = c (in -
    /// y1) + x1` with no history), so through two stages as its square: the render shows it
    /// directly, on the stream's lattice from the first block.
    #[test]
    fn between_corners_the_coefficient_is_the_chord_read_a_step_ahead() {
        let band = SweepRange::of(Hz::new(300.0), Hz::new(3_000.0));
        let c = controls(5.0, 1.0, 0.0);
        for n in [0usize, 1, 7, 15, 16, 17, 31, 32, 40, 47] {
            let mut k = kernel(1, 2, band, Stagger::NONE);
            let mut x = vec![0.0f32; n + 1];
            x[n] = 1.0;
            let y = drive_kernel(&mut k, &[(0, c)], &[x]).remove(0);
            let (j, base) = (n % 16, n - n % 16);
            let prev = exact_complement(band, 5.0, 1.0, 0.0, base);
            let next = exact_complement(band, 5.0, 1.0, 0.0, base + 16);
            let chord = prev + (next - prev) * ((j + 1) as f32 / 16.0) - 1.0;
            assert!(
                (y[n] - chord * chord).abs() < 1e-6,
                "frame {n}: {} against the chord's square {}",
                y[n],
                chord * chord
            );
        }
    }

    /// A depth that ramps is solved into a segment's end corner with the depth the ramp
    /// has **there**, sixteen steps on: a depth set from 0 to 1 over 240 steps at the
    /// grid point at frame 16 has its start corner at depth 0 (the last segment's end) and
    /// its end corner at `16 / 240`.
    #[test]
    fn a_ramping_depth_is_solved_with_the_depth_the_ramp_has_at_the_segments_end() {
        let band = SweepRange::of(Hz::new(300.0), Hz::new(3_000.0));
        let mut k = kernel(1, 4, band, Stagger::NONE);
        let moves = [(0, controls(5.0, 0.0, 0.5)), (16, controls(5.0, 1.0, 0.5))];
        drive_kernel(&mut k, &moves, &silence(1, 17));
        let start = exact_complement(band, 5.0, 0.0, 0.0, 16);
        let end = exact_complement(band, 5.0, 16.0 / 240.0, 0.0, 32);
        // The retarget at 16 solved the segment 16..32; the frame rendered since (frame 16)
        // took one step.
        let g = k.complements[0];
        assert!(
            (g.target() - end).abs() < 1e-6,
            "the end: {} against {end}",
            g.target()
        );
        let first = start + (end - start) / 16.0;
        assert!(
            (g.now() - first).abs() < 1e-6,
            "one step on the chord from {start}"
        );
    }

    /// The depth and the feedback ramp over `GAIN_RAMP` (240 frames at 48 kHz) from where
    /// they were, halfway at the middle frame and landing exactly: not at the block, and not
    /// in one step.
    #[test]
    fn depth_and_feedback_ramp_over_the_gain_ramp_and_land_exactly() {
        let moves = [(0, controls(1.0, 0.0, 0.2)), (16, controls(1.0, 1.0, 0.8))];
        let after = |frames: usize| {
            let mut k = kernel(1, 4, SweepRange::DEFAULT, Stagger::NONE);
            drive_kernel(&mut k, &moves, &silence(1, frames));
            k
        };
        // The ramp was set to frame 16: 120 frames on is half way.
        let half = after(16 + 120);
        assert!((half.depth.now() - 0.5).abs() < 1e-6, "depth halfway");
        assert!((half.feedback.now() - 0.5).abs() < 1e-6, "feedback halfway");
        let arrived = after(16 + 240);
        assert_eq!((arrived.depth.now(), arrived.feedback.now()), (1.0, 0.8));
        assert!(!arrived.depth.moving() && !arrived.feedback.moving());
    }

    /// The rate is floored at 0.01 Hz: a rate of -3 Hz, of zero and of 0.005 Hz is the
    /// 0.01 Hz sweep, whose LFO phase is the walk of `0.01 / sr` a frame (walked here
    /// per frame, not read off the kernel), and a rate above the floor is not. 4801 frames
    /// solve the segments at 0, 16, .., 4800: 301 of 16 steps.
    #[test]
    fn a_rate_below_the_floor_is_the_floor() {
        let walked = |hz: f64, steps: usize| {
            let step = PhaseStep::from(PhaseIncrement::per_sample(Hz::new(hz), RATE));
            (0..steps).fold(RunningPhase::START, |p, _| p.advance(step))
        };
        for (rate, effective) in [
            (-3.0f64, 0.01),
            (0.0, 0.01),
            (0.005, 0.01),
            (0.01, 0.01),
            (0.5, 0.5),
        ] {
            let mut k = kernel(1, 4, SweepRange::DEFAULT, Stagger::NONE);
            drive_kernel(&mut k, &[(0, controls(rate, 1.0, 0.5))], &silence(1, 4_801));
            assert_eq!(
                k.clock.state().0,
                walked(effective, 4_800 + 16),
                "rate {rate}"
            );
        }
        assert_ne!(walked(0.5, 4_816), walked(0.01, 4_816));
    }

    /// The feedback is clamped into `0..=MAX_STABLE`, whatever is asked.
    #[test]
    fn a_feedback_is_clamped_into_the_stable_range() {
        let mut k = kernel(1, 4, SweepRange::DEFAULT, Stagger::NONE);
        k.retarget(&controls(1.0, 0.5, 5.0));
        assert_eq!(k.feedback.target(), f32::from_unit(Feedback::MAX_STABLE));
        k.retarget(&controls(1.0, 0.5, -0.5));
        assert_eq!(k.feedback.target(), 0.0);
        // The depth is as typed: the whole range, and inverted, the bottom and below.
        k.retarget(&controls(1.0, 1.0, 0.5));
        assert_eq!(k.depth.target(), f32::from_unit(Depth::FULL));
        k.retarget(&controls(1.0, -1.0, 0.5));
        assert_eq!(k.depth.target(), f32::from_unit(Depth::INVERTED));
    }

    /// The sweep's top is held at 0.90 of Nyquist at the rate, and the authored top is kept:
    /// a low rate caps it, a higher rate after it restores it (a cap that ratcheted the top
    /// down for good would stay at 3.6 kHz).
    #[test]
    fn the_range_is_capped_at_0_9_nyquist_and_a_rising_rate_restores_the_authored_top() {
        let top = |range: SweepRange, sr: f64| {
            let mut k = kernel(1, 4, range, Stagger::NONE);
            k.prepare(SampleRate::of(sr), MaxBlock::of(Samples::new(64)));
            (k.min, k.max)
        };
        assert_eq!(
            top(SweepRange::DEFAULT, 8_000.0).1,
            Hz::new(3_600.0),
            "0.90 of a 4 kHz Nyquist"
        );
        assert_eq!(top(SweepRange::DEFAULT, 48_000.0).1, Hz::new(4_000.0));
        let wide = SweepRange::of(Hz::new(300.0), Hz::new(30_000.0));
        assert_eq!(top(wide, 48_000.0), (Hz::new(300.0), Hz::new(21_600.0)));
        assert_eq!(top(wide, 96_000.0).1, Hz::new(30_000.0));
        let mut k = kernel(1, 4, wide, Stagger::NONE);
        k.prepare(SampleRate::of(48_000.0), MaxBlock::of(Samples::new(64)));
        k.prepare(SampleRate::of(96_000.0), MaxBlock::of(Samples::new(64)));
        assert_eq!(k.max, Hz::new(30_000.0));
    }

    /// A rate that moves lands at its **grid point** and the LFO's phase is continuous
    /// across it: the phase is one walk, the old rate's step until the first grid point at
    /// or after the move and the new rate's from there. A move anywhere after frame 32 up to
    /// the point at 48 gives the same walk; at 32 itself the segment at 32 already takes the
    /// new rate.
    #[test]
    fn a_rate_change_lands_at_its_grid_point_and_the_phase_stays_continuous() {
        let (slow, fast) = (controls(2.0, 1.0, 0.5), controls(9.0, 1.0, 0.5));
        let inc = |hz: f64| PhaseStep::from(PhaseIncrement::per_sample(Hz::new(hz), RATE));
        let walk = |first: usize, second: usize| {
            let p = (0..first).fold(RunningPhase::START, |p, _| p.advance(inc(2.0)));
            (0..second).fold(p, |p, _| p.advance(inc(9.0)))
        };
        let after = |at: usize| {
            let mut k = kernel(1, 4, SweepRange::DEFAULT, Stagger::NONE);
            // To frame 70: the segments at 0, 16, 32, 48 and 64 are solved.
            drive_kernel(&mut k, &[(0, slow), (at, fast)], &silence(1, 70));
            k.clock.state().0
        };
        for at in [33, 40, 47, 48] {
            assert_eq!(after(at), walk(48, 32), "moved at frame {at}");
        }
        assert_eq!(after(32), walk(32, 48), "moved at frame 32");
        assert_ne!(walk(48, 32), walk(32, 48));
    }

    /// A re-prepare restarts the sweep and empties the chain (the coefficient was the old
    /// rate's): the LFO is at its start again, no segment is solved and silence in is silence
    /// out.
    #[test]
    fn a_re_prepare_restarts_the_sweep() {
        let mut k = kernel(
            2,
            4,
            SweepRange::DEFAULT,
            Stagger::of_step(PhaseIncrement::new(0.25)),
        );
        let loud = vec![vec![1.0f32; 1_000]; 2];
        drive_kernel(&mut k, &[(0, controls(3.0, 1.0, 0.7))], &loud);
        assert_ne!(
            k.clock.state().0,
            RunningPhase::START,
            "the sweep was running"
        );
        k.prepare(RATE, MaxBlock::of(Samples::new(64)));
        assert_eq!(k.clock.state(), (RunningPhase::START, 0, false));
        let y = drive_kernel(&mut k, &[(0, controls(3.0, 1.0, 0.7))], &silence(2, 200));
        assert!(y.iter().flatten().all(|&s| s == 0.0));
    }

    /// A reset restores the sweep's start: the same stream rendered after a reset, at the
    /// same controls, is the fresh node's bit for bit, though the reset came at a frame that
    /// is not a grid point.
    #[test]
    fn a_reset_node_renders_what_a_fresh_one_does() {
        let x: Vec<f32> = (0..400)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let c = controls(7.0, 0.8, 0.6);
        let blocks = |node: &mut PhaserNode<f32>, from: usize| -> Vec<f32> {
            let mut out = Vec::new();
            for chunk in x[from..].chunks(37) {
                out.extend(drive(node, RATE, Planes::new(&[chunk, chunk]).unwrap(), &[]).remove(0));
            }
            out
        };
        let mut used = prepared(
            PhaserNode::<f32>::try_from(
                PhaserConfig::new(BusTopology::STEREO, Stages::of(4)).controls(c),
            )
            .unwrap(),
            RATE,
            64,
        );
        blocks(&mut used, 100);
        used.reset();
        let mut fresh = prepared(
            PhaserNode::<f32>::try_from(
                PhaserConfig::new(BusTopology::STEREO, Stages::of(4)).controls(c),
            )
            .unwrap(),
            RATE,
            64,
        );
        assert_eq!(blocks(&mut used, 0), blocks(&mut fresh, 0));
    }

    /// A cell that moves **between** grid points starts its ramp at the next grid point: a
    /// rate, a depth and a feedback written 13 frames past frame 1024 and heard from the
    /// block that begins there render the same as the same write at frame 1040, the next
    /// grid point, and differently from the write at 1056.
    #[test]
    fn a_move_between_grid_points_starts_at_the_next_one() {
        let len = 2_048;
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |at: usize| -> Vec<f32> {
            let node =
                PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::MONO, Stages::of(6)))
                    .unwrap();
            let set = node.param_set();
            let mut node = prepared(node, RATE, 2_048);
            let mut out = Vec::new();
            for (chunk, from) in [(&x[..at], 0), (&x[at..], at)] {
                if from == at {
                    assert!(set.set(UnitParam::Rate, 8.0));
                    assert!(set.set(UnitParam::Depth, 0.2));
                    assert!(set.set(UnitParam::Feedback, 0.1));
                }
                out.extend(drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &[]).remove(0));
            }
            out
        };
        let at_the_point = render(1_040);
        assert_eq!(render(1_037), at_the_point);
        assert_ne!(render(1_056), at_the_point);
    }

    /// A reset node equals a fresh one that has the cell: render, move the depth and the
    /// feedback, reset, and the render is the fresh node's with those cells (the move lands at
    /// once, it does not ramp from the old value).
    #[test]
    fn a_cell_moved_before_a_reset_lands_at_once_as_on_a_fresh_node() {
        let x: Vec<f32> = (0..1_000)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let moved = |node: &PhaserNode<f32>| {
            let set = node.param_set();
            assert!(set.set(UnitParam::Depth, 0.9));
            assert!(set.set(UnitParam::Feedback, 0.8));
        };
        let blocks = |node: &mut PhaserNode<f32>| -> Vec<f32> {
            x.chunks(64)
                .flat_map(|c| drive(node, RATE, Planes::new(&[c]).unwrap(), &[]).remove(0))
                .collect()
        };
        let mut used = prepared(
            PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::MONO, Stages::of(4)))
                .unwrap(),
            RATE,
            64,
        );
        blocks(&mut used);
        moved(&used);
        used.reset();
        let fresh =
            PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::MONO, Stages::of(4)))
                .unwrap();
        moved(&fresh);
        let mut fresh = prepared(fresh, RATE, 64);
        assert_eq!(blocks(&mut used), blocks(&mut fresh));
    }

    /// An `f64` block through a `PhaserKernel::<f64>` at depth 0 (the sweep parked at the
    /// range's bottom, so the complement `a = 1 + c` is the one design for 200 Hz) is, per frame,
    /// the stage difference equation `s[n] = c (u[n] - s[n - 1]) + u[n - 1]` twice in cascade on
    /// `u[n] = x[n] + g y[n - 1]`, computed in `f64`, to 2^-45.
    #[test]
    fn an_f64_kernel_at_no_depth_is_the_all_pass_difference_equation_in_f64() {
        use tutti_fixtures::signal::{assert_close_f64, wide_signal};
        let (stages, len, g) = (2usize, 300usize, 0.5f64);
        let a: f64 = allpass_complement(SweepRange::DEFAULT.min(), RATE);
        let c = a - 1.0;
        assert_ne!(
            a,
            f64::from(a as f32),
            "the coefficient needs more than f32 holds"
        );
        let x = wide_signal(len);
        let mut want = vec![0.0f64; len];
        let mut state = vec![(0.0f64, 0.0f64); stages];
        let mut last = 0.0;
        for n in 0..len {
            let mut u = x[n] + g * last;
            for (x1, y1) in &mut state {
                let s = c * (u - *y1) + *x1;
                (*x1, *y1) = (u, s);
                u = s;
            }
            last = u;
            want[n] = u;
        }
        let controls = PhaserControls {
            depth: Depth::of(0.0),
            feedback: Feedback::new(0.5),
            ..PhaserControls::DEFAULT
        };
        let mut k = PhaserKernel::<f64>::new(
            BusTopology::MONO,
            Stages::of(stages),
            SweepRange::DEFAULT,
            Stagger::NONE,
        )
        .unwrap();
        k.prepare(RATE, MaxBlock::of(Samples::new(len)));
        let y = drive_kernel(&mut k, &[(0, controls)], &[x]).remove(0);
        assert_close_f64(&y, &want, 2.0f64.powi(-45));
    }
}
