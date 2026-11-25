//! Chorus and flanger: one width-generic swept delay, two presets.
//!
//! [`ModDelayBank`] is the algorithm (the lines and the loop); this file holds the sweep (the
//! LFO and the solve of each channel's delay), the cells and how the graph reaches them. A
//! chorus sits at a long base delay (10 ms) so the copy reads as a second voice thickening the
//! sound, a flanger at a short one (1 ms) so the copy combs against the original and the sweep
//! moves the notches: the same node at different numbers ([`Sweep`]), at any width.

use tutti_buffers::Float;
use tutti_buffers::{ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, DELAY_SLEW, GAIN_RAMP, Lerp, ModDelayBank, ModDelayControls,
    ModDelayLayout, ModulationRate, Ramp, Steps, Sweep,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Cents, Feedback, Hz, SampleRate, SampleSpan, Seconds, Tail};

#[cfg(doc)]
use tutti_dsp::Stagger;

use super::sweep_clock::{SEGMENT, SweepClock};
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// A swept delay of any width, a chorus or a flanger by its [`Sweep`].
///
/// A [`KernelNode`] over [`ModDelayKernel`]: each channel has its own delay line swept by
/// one LFO, staggered per channel by a [`Stagger`] (channel `c` runs `c` times a step ahead
/// of channel 0, or an offset of its own). At width 2 with the [`chorus`](ModDelayConfig::chorus)
/// or [`flanger`](ModDelayConfig::flanger) preset it is a stereo chorus or flanger.
///
/// Made from a [`ModDelayConfig`] (`ModDelayNode::from`) on a checked [`ChannelTopology`] (an
/// `n`-channel one on the engine's order for that width is `ChannelTopology::default_for(n)?`, which
/// refuses a width past one bus): a preset starting at its controls
/// ([`ModDelayControls::CHORUS`] and kin), or a [`Sweep`] and a [`Stagger`] of your own.
///
/// # The controls
///
/// The ports are in this order: **rate** ([`Rate`](tutti_units::UnitParam::Rate), [`Hz`]),
/// **depth** ([`Depth`](tutti_units::UnitParam::Depth), [`Seconds`] of delay sweep) and
/// [`Feedback`]. All three are read on the stream's 16-frame grid, so what is heard does not
/// depend on how the stream is cut into blocks; a driven port is read at the grid's solve
/// frame.
///
/// - The **rate** is how fast the delay sweeps: chorus and flanger rates are slow, typically
///   0.1 to 2 Hz, and faster reads as vibrato. **Floored at 0.01 Hz, a control signal's value and a
///   cell's alike.** It lands whole, and the LFO's phase is continuous across it.
/// - The **depth** is a duration added to the base delay, unlike the phaser's unitless depth;
///   it is clamped into the sweep's range. A cell that moves ramps, at no more than
///   [`DELAY_SLEW`] frames of delay per frame: a moving delay is a pitch bend, so it is its
///   speed that is bounded.
/// - The **feedback** is how much of the delayed signal recirculates, clamped into the
///   sweep's range; on a flanger it is the most characterful control. A cell that moves
///   ramps over [`GAIN_RAMP`].
///
/// The output is the wet signal alone; the blend with the dry input is the graph's
/// (`Editor::set_mix`; the node has no latency, so its aligned input is its input). At fully
/// wet a chorus's detuned copy stands alone and a flanger's notches (wet and dry
/// interfering) vanish, so both effects want a mix below one.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)), `N` in and `N` out, with zero latency:
/// the modulated delay is the effect's sound, not processing latency, and PDC would otherwise
/// delay every other path by the base delay. Inserted, its controls are a
/// [`ParamSet`](tutti_graph::ParamSet) over those three; a fork starts from the values last
/// set through it, with the LFO back at the start. The graph prepares it at the device rate,
/// which sizes the lines, before its first block, and a re-prepare restarts the sweep.
///
/// # Tail
///
/// [`Tail::Unknown`] (a recirculating line), so it is never skipped, unless the sweep's
/// [`feedback_max`](Sweep::feedback_max) is [`Feedback::NONE`] (the General MIDI chorus):
/// then nothing recirculates, an output frame is made of input at most
/// [`max_delay`](Sweep::max_delay) old, and it declares that as its finite tail.
///
/// `F` is the float of the graph it runs in, and of the lines and the ramps; a line holds
/// `ceil(max_delay * rate)` frames of it, so an `f32` graph halves the memory of an `f64` one.
pub type ModDelayNode<F> = KernelNode<ModDelayKernel<F>>;

/// What a [`ModDelayNode`] is made from: the [`ModDelayLayout`] of its lines and the controls it
/// starts at.
///
/// The layout holds the bus, the [`Sweep`] and the [`Stagger`] of its channels, checked against
/// each other where it is built
/// ([`ModDelayLayoutBuilder::build`](tutti_dsp::ModDelayLayoutBuilder::build)).
/// [`chorus`](Self::chorus), [`flanger`](Self::flanger) and
/// [`general_midi_chorus`](Self::general_midi_chorus) take the layout and the controls
/// together, as [`ModDelayLayout`]'s presets and [`ModDelayControls`]' constants have them.
#[derive(Clone, Copy, Debug)]
pub struct ModDelayConfig {
    layout: ModDelayLayout,
    controls: ModDelayControls,
}

impl ModDelayConfig {
    /// A swept delay built to `layout`, at [`ModDelayControls::DEFAULT`].
    #[must_use]
    pub const fn new(layout: ModDelayLayout) -> Self {
        Self {
            layout,
            controls: ModDelayControls::DEFAULT,
        }
    }

    /// A chorus on the bus `input`: [`ModDelayLayout::chorus`] at
    /// [`ModDelayControls::CHORUS`].
    #[must_use]
    pub fn chorus(input: ChannelTopology) -> Self {
        Self::new(ModDelayLayout::chorus(input)).controls(ModDelayControls::CHORUS)
    }

    /// A flanger on the bus `input`: [`ModDelayLayout::flanger`] at
    /// [`ModDelayControls::FLANGER`].
    #[must_use]
    pub fn flanger(input: ChannelTopology) -> Self {
        Self::new(ModDelayLayout::flanger(input)).controls(ModDelayControls::FLANGER)
    }

    /// A vibrato on the bus `input`: [`ModDelayLayout::vibrato`] at 5 Hz and a swing of 50 cents
    /// ([`Sweep::vibrato_controls`]). It has no feedback, so its node declares a finite
    /// tail, and it is the wet signal alone: the pitch swing is the whole effect, with no
    /// dry path to blend.
    ///
    /// # Panics
    ///
    /// Never: 50 cents at 5 Hz is a 0.9 ms depth, inside [`Sweep::VIBRATO`]'s range.
    #[must_use]
    pub fn vibrato(input: ChannelTopology) -> Self {
        Self::new(ModDelayLayout::vibrato(input)).controls(
            Sweep::VIBRATO
                .vibrato_controls(Cents::new(50.0), ModulationRate::of(Hz::new(5.0)))
                .expect("50 cents at 5 Hz is a 0.9 ms depth, inside Sweep::VIBRATO's 0..=12 ms"),
        )
    }

    /// The General MIDI chorus on the bus `input`: [`ModDelayLayout::general_midi_chorus`] at
    /// [`ModDelayControls::GENERAL_MIDI_CHORUS`]. It has no feedback, so its node declares a
    /// finite tail.
    #[must_use]
    pub fn general_midi_chorus(input: ChannelTopology) -> Self {
        Self::new(ModDelayLayout::general_midi_chorus(input))
            .controls(ModDelayControls::GENERAL_MIDI_CHORUS)
    }

    /// The controls the cells start at.
    #[must_use]
    pub const fn controls(mut self, controls: ModDelayControls) -> Self {
        self.controls = controls;
        self
    }
}

impl<F: Float + Lerp> From<ModDelayConfig> for ModDelayNode<F> {
    /// The lines are sized at the rate [`Node::prepare`](tutti_graph::Node::prepare) hands it,
    /// which is what makes the longest delay hold at whatever rate the graph runs at.
    /// Allocates.
    fn from(config: ModDelayConfig) -> Self {
        Self::with_kernel(
            ModDelayKernel::new(config.layout),
            ModDelaySet,
            config.controls,
        )
    }
}

/// The swept delay's controls, in port order: rate, depth, feedback. Each lands whole on the
/// stream grid (the kernel's own ramps are the depth's, at a pitch bend's speed, and the
/// feedback's: the reader must not ramp them across the block as well).
#[derive(Clone, Copy, Debug)]
pub struct ModDelaySet;

control_set! {
    ModDelaySet(_s): ModDelayControls = ModDelayControls::DEFAULT;
    live: (all);
    // How fast the delay sweeps: floored at 0.01 Hz, a control signal's value and a cell's alike.
    Rate => Hz = rate { whole },
    // The swing, in seconds of delay.
    Depth => Seconds = depth { whole },
    Feedback => Feedback = feedback { whole };

    fn defaults(&self) -> ModDelayControls {
        ModDelayControls::DEFAULT
    }
}

/// The swept delay as a [`Kernel`]: a [`ModDelayBank`], the LFO that sweeps it
/// (`SweepClock`) and the solve of each channel's delay.
///
/// The LFO's corners are solved every [`COEFF_INTERVAL`] frames of the stream (a sine per
/// corner and channel) and each channel's delay ramps between them, so the cut of the blocks
/// does not move it: the render is the same whatever the blocks. The chord of a sine over a
/// step of `h` radians misses it by at most `h^2 / 8` of the depth, 0.03 frames against the
/// deepest chorus at 5 Hz and 48 kHz. The LFO steps before the frame it sounds in: frame `i`
/// of a segment sounds the sweep `i + 1` steps on from the segment's start. **A re-prepare
/// restarts the sweep**; a depth and a feedback that move start their ramps when they move,
/// and the rate is taken at the next grid point, with the phase continuous across it.
///
/// **A moving depth is a pitch bend**, so its speed is bounded, not its duration: a depth that
/// moves ramps at most [`DELAY_SLEW`] frames of delay per frame, and is solved into a
/// segment's end corner with the depth the ramp has there. The feedback ramps over
/// [`GAIN_RAMP`].
///
/// The delay has **no latency**: the sweep is the sound, not processing delay. Its tail is
/// [`Tail::Unknown`] (a recirculating line), or, for a sweep that cannot recirculate, its
/// longest delay.
#[derive(Clone, Debug)]
pub struct ModDelayKernel<F: Float> {
    layout: ModDelayLayout,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<ModDelayBank<F>>,
    /// The ramp length of the feedback: [`GAIN_RAMP`] at the prepared rate.
    steps: u32,
    /// Each channel's excursion from the base delay, in frames, across the segment in flight,
    /// from its start corner.
    delays: Box<[Ramp<F>]>,
    /// The depth in frames of delay.
    depth: Ramp<F>,
    feedback: Ramp<F>,
    clock: SweepClock,
}

impl<F: Float + Lerp> ModDelayKernel<F> {
    /// A swept delay built to `layout`. It has no lines until `prepare` gives it a rate.
    fn new(layout: ModDelayLayout) -> Self {
        let d = ModDelayControls::DEFAULT;
        Self {
            bank: None,
            steps: 0,
            delays: vec![Ramp::new(F::ZERO); layout.width()].into_boxed_slice(),
            depth: Ramp::new(F::ZERO),
            feedback: Ramp::new(F::from_unit(d.feedback)),
            clock: SweepClock::new(layout.offsets()),
            layout,
        }
    }

    /// Solves the next segment: its start corner is the last one's end (or the LFO's start
    /// after a reset), its end corner the sweep [`COEFF_INTERVAL`] frames on at the LFO's
    /// step and the depth the ramp has there.
    fn solve(&mut self) {
        let Some(bank) = &self.bank else {
            return;
        };
        let span = |frames: F| SampleSpan::clamped(frames.into());
        let depth_start = span(self.depth.now());
        let depth_end = span(
            self.depth
                .at(self.depth.steps_taken().saturating_add(SEGMENT)),
        );
        self.clock.solve(&mut self.delays, |lfo, end| {
            let depth = if end { depth_end } else { depth_start };
            bank.swept(lfo, depth)
        });
    }
}

impl<F: Float + Lerp> Kernel for ModDelayKernel<F> {
    type Float = F;
    type Set = ModDelaySet;

    /// The corners solve on the stream's grid of 16 frames, like the filters and the phaser: a
    /// cell that moves inside a block reaches the render within 16 frames, wherever the
    /// blocks fall.
    const GRID: usize = COEFF_INTERVAL;

    /// The LFO moves every frame, so every grid point solves a segment.
    const CUT_ALWAYS: bool = true;

    fn shape(&self) -> Shape {
        let input = self.layout.input();
        Shape::through(input).cost(Cost::Moderate)
    }

    /// Sizes a line per channel at `rate` (`ceil(max_delay * rate)` frames: a re-prepare
    /// empties them, the delays in frames having been the old rate's) and the feedback ramp
    /// from [`GAIN_RAMP`] at it, the conversion saturating: a prepare must not panic. The
    /// sweep restarts.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        self.bank = Some(ModDelayBank::<F>::new(&self.layout, rate));
        self.reset();
    }

    /// Back to silence with the sweep at its start: the lines empty, the ramps settled and the
    /// next design landing at once, as a fresh node's first does.
    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
        self.depth.jump_to_target();
        self.feedback.jump_to_target();
        for delay in &mut self.delays {
            delay.jump_to_target();
        }
        self.clock.reset();
    }

    /// Aims the depth and the feedback at `controls` and takes the rate's step, whenever a
    /// control moves: the ramps start at once and the segment is solved at its corner, from
    /// the depth the ramp has there. The first design after a prepare or a reset lands in
    /// [`jump_to_target`](Kernel::jump_to_target) before the first segment is solved.
    fn retarget(&mut self, controls: &ModDelayControls) {
        let Some(bank) = &self.bank else { return };
        let design = bank.design(controls);
        self.clock.set_inc(design.inc);
        self.depth
            .retarget_by(F::from_unit(design.depth), Steps::Rate(DELAY_SLEW));
        self.feedback
            .retarget_by(F::from_unit(design.feedback), Steps::Fixed(self.steps));
    }

    /// The depth and the feedback on their targets. The delays stay on their segment: the
    /// sweep is the LFO's, not a design.
    fn jump_to_target(&mut self) {
        self.depth.jump_to_target();
        self.feedback.jump_to_target();
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        if self.clock.due() {
            self.solve();
        }
        let frames = block.frames().get();
        let rendered = match &mut self.bank {
            Some(bank) => bank.process(&mut self.delays, &mut self.feedback, block),
            None => Ok(()),
        };
        self.depth
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        self.clock.advance(frames);
        rendered
    }

    /// A recirculating line rings for as long as its feedback says: unknown, so the graph
    /// never reads it as none. A sweep that cannot recirculate holds an input frame at most
    /// its longest delay.
    fn tail(&self) -> Tail {
        match &self.bank {
            Some(bank) if self.layout.sweep().feedback_max() == Feedback::NONE => {
                Tail::Finite(bank.max_delay())
            }
            _ => Tail::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use tutti_buffers::Planes;
    // The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
    // format, so their tolerances are `f32`'s.
    use super::*;
    use crate::test_support::drive_kernel;
    use tutti_buffers::{ChannelLayout, MaxBlock};
    use tutti_dsp::{MaxDelay, Stagger};
    use tutti_graph::contract::{assert_declared_tail, assert_param_fork, drive, prepared};
    use tutti_graph::{Node, ParamNode};
    use tutti_units::{Latency, Real, Samples, UnitParam};
    use tutti_units::{Phase, PhaseIncrement, RunningPhase};

    const RATE: SampleRate = SampleRate::SR_48K;
    const SR: f64 = 48_000.0;

    fn bus(width: u16) -> ChannelTopology {
        ChannelTopology::default_for(ChannelLayout::from(width)).expect("a test width is one bus")
    }

    fn controls(rate: f64, depth: f64, feedback: f64) -> ModDelayControls {
        ModDelayControls {
            rate: Hz::new(rate),
            depth: Seconds::new(depth),
            feedback: Feedback::new(feedback),
        }
    }

    // ---- the node: ports, shape, fork -------------------------------------------------

    /// A fork starts from the values last set through the node's `ParamSet` and shares no
    /// cell with it, and the set has the three controls in the layout's order.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = ModDelayNode::<f32>::from(ModDelayConfig::flanger(ChannelTopology::STEREO));
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Rate, UnitParam::Depth, UnitParam::Feedback]
        );
        assert_param_fork(node);
    }

    /// The node declares rate, depth and feedback as its params in that order, each with its
    /// control as the base, as wide as it was built, in and out, with no latency (the sweep
    /// is the sound, not processing delay). A recirculating line's tail is unknown; the
    /// General MIDI chorus, which cannot recirculate, declares its longest delay, 5 ms, as a
    /// finite ceiling.
    #[test]
    fn the_shape_declares_the_three_controls_no_latency_and_its_tail() {
        let chorus = ModDelayNode::<f32>::from(ModDelayConfig::chorus(bus(3)));
        let shape = chorus.shape();
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
        assert_eq!(
            chorus.param_base(0),
            Some(1.0),
            "the rate's base is its cell"
        );
        assert_eq!(chorus.param_base(1), Some(0.005));
        assert_eq!(chorus.param_base(2), Some(0.3));
        assert_eq!(chorus.param_base(3), None, "no fourth param");
        let flanger = ModDelayNode::<f32>::from(ModDelayConfig::flanger(ChannelTopology::MONO));
        assert_eq!(flanger.shape().tail, Tail::Unknown);
        let mut gm = prepared(
            ModDelayNode::<f32>::from(ModDelayConfig::general_midi_chorus(ChannelTopology::STEREO)),
            RATE,
            64,
        );
        assert_eq!(gm.shape().tail, Tail::Finite(Samples::new(240)));
        assert_eq!(gm.shape().latency, Latency::new(Samples::new(0)));
        assert_declared_tail(&mut gm, RATE);
    }

    // ---- the kernel: segments, corners and the sweep ----------------------------------

    /// A mono sweep of the trajectory tests: base 20 ms, up to 40 ms deep in an 80 ms line.
    /// At 20 Hz and 10 ms it is the worst case for the chord between corners.
    fn trajectory_layout(width: u16, stagger: Stagger) -> ModDelayLayout {
        let sweep = Sweep::of(
            Seconds::new(0.02),
            MaxDelay::of(Seconds::new(0.08)),
            Seconds::new(0.0),
            Seconds::new(0.04),
            Feedback::MAX_STABLE,
        );
        ModDelayLayout::builder(bus(width), sweep)
            .stagger(stagger)
            .build()
            .expect("a stagger that fits")
    }

    fn prepared_kernel(layout: ModDelayLayout, rate: SampleRate) -> ModDelayKernel<f32> {
        let mut k = ModDelayKernel::<f32>::new(layout);
        k.prepare(rate, MaxBlock::of(Samples::new(64)));
        k
    }

    fn trajectory() -> ModDelayKernel<f32> {
        prepared_kernel(trajectory_layout(1, Stagger::NONE), RATE)
    }

    /// The delay in use at every frame of channel 0, in frames, by feeding the ramp
    /// `x[n] = n` through the kernel: a linearly interpolated read of a ramp is exact, so
    /// `n - y[n]` is the delay the read used.
    fn trace(
        k: &mut ModDelayKernel<f32>,
        moves: &[(usize, ModDelayControls)],
        len: usize,
    ) -> Vec<f64> {
        let x: Vec<f32> = (0..len).map(|n| n as f32).collect();
        let y = drive_kernel(k, moves, &[x]).remove(0);
        y.iter()
            .enumerate()
            .map(|(n, &v)| n as f64 - f64::from(v))
            .collect()
    }

    /// The first frame whose delay line is full of the ramp.
    const FROM: usize = 1_600;
    const BASE: f64 = 0.02 * SR;

    /// The exact sweep's delay at every frame, in frames, from the sweep's definition
    /// `base + depth sin(2 pi phase)`: the sine of the LFO's own phase. The LFO steps before the frame it sounds in: frame `n` sounds the phase `n + 1`
    /// steps on, so the corners (frame `16 k + 15`) are the sine itself. `rates` are `(frame,
    /// hz)`: the rate in force from that frame on.
    fn exact_delays(len: usize, depth: f64, rate: f64, rates: &[(usize, f64)]) -> Vec<f64> {
        let mut inc = PhaseIncrement::per_sample(Hz::new(rate), RATE);
        let mut phase = Phase::START;
        (0..len)
            .map(|n| {
                if let Some(&(_, hz)) = rates.iter().find(|r| r.0 == n) {
                    inc = PhaseIncrement::per_sample(Hz::new(hz), RATE);
                }
                phase = phase.advance(inc);
                let turns = phase.get();
                BASE + depth * SR * (std::f64::consts::TAU * turns).sin()
            })
            .collect()
    }

    /// The chord bound of a sine over one control interval, as a delay in frames: `h^2 / 8`
    /// of the depth, `h` the radians the interval spans.
    fn chord_bound(depth: f64, hz: f64) -> f64 {
        let h = std::f64::consts::TAU * hz * COEFF_INTERVAL as f64 / SR;
        depth * SR * h * h / 8.0
    }

    /// A held depth of zero is a pure delay of the declared span: the 20 ms base is 960
    /// frames, so a ramp comes out as the ramp 960 frames late and an impulse at frame 960
    /// exactly, whatever the rate says, with no feedback to add an echo.
    #[test]
    fn a_held_depth_of_zero_is_a_pure_delay_of_the_declared_span() {
        for hz in [0.01f64, 1.0, 37.0] {
            let mut k = trajectory();
            let d = trace(&mut k, &[(0, controls(hz, 0.0, 0.0))], 3_000);
            for (n, &got) in d.iter().enumerate().skip(1_000) {
                assert_eq!(got, 960.0, "rate {hz}, frame {n}");
            }
        }
        let mut k = trajectory();
        let mut x = vec![0.0f32; 4_000];
        x[0] = 1.0;
        let y = drive_kernel(&mut k, &[(0, controls(2.0, 0.0, 0.0))], &[x]).remove(0);
        for (n, &v) in y.iter().enumerate() {
            assert_eq!(v, if n == 960 { 1.0 } else { 0.0 }, "frame {n}");
        }
    }

    /// The delays a sweep reaches are `[max(1, base - depth), base + depth]` frames, an
    /// analytic figure of the definition: what is rendered never leaves `[base - depth, base +
    /// depth]` and reaches both ends.
    #[test]
    fn the_rendered_delay_stays_within_the_sweeps_range_and_reaches_it() {
        let depth = 0.01f64;
        let amp = depth * SR;
        let mut k = trajectory();
        let trace = trace(&mut k, &[(0, controls(20.0, depth, 0.0))], 12_000);
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for &d in &trace[FROM..] {
            let e = d - BASE;
            assert!(e.abs() <= amp + 0.01, "{e} past +-{amp}");
            lo = lo.min(e);
            hi = hi.max(e);
        }
        assert!(hi > amp * 0.999 && lo < -amp * 0.999, "{lo}..{hi}");
    }

    /// At a control point (the last frame of every segment of 16, counted from the start) the
    /// delay is the exact sine's, `base + depth sin(2 pi phase)`; between them the chord misses it by at most
    /// `h^2 / 8` of the depth, and the chord does miss (not vacuous).
    #[test]
    fn the_delay_is_the_exact_sine_at_control_points_and_within_the_chord_between() {
        let (depth, hz) = (0.01f64, 20.0f64);
        let mut k = trajectory();
        let trace = trace(&mut k, &[(0, controls(hz, depth, 0.0))], 12_000);
        let exact = exact_delays(12_000, depth, hz, &[]);
        for n in (FROM..12_000).filter(|n| (n + 1).is_multiple_of(COEFF_INTERVAL)) {
            let err = trace[n] - exact[n];
            assert!(err.abs() < 0.05, "frame {n}: off by {err}");
        }
        let bound = chord_bound(depth, hz);
        let worst = (FROM..12_000)
            .map(|n| (trace[n] - exact[n]).abs())
            .fold(0.0, f64::max);
        assert!(worst <= bound + 0.05, "worst {worst} over bound {bound}");
        assert!(worst > bound * 0.5, "worst {worst}, bound {bound}");
    }

    /// The sweep keeps its period: successive upward crossings of the centre are `sr / rate`
    /// apart.
    #[test]
    fn the_sweeps_period_is_one_over_the_rate() {
        let (depth, hz) = (0.01f64, 20.0f64);
        let mut k = trajectory();
        let trace = trace(&mut k, &[(0, controls(hz, depth, 0.0))], 16_000);
        let mut ups = Vec::new();
        for n in FROM..15_999 {
            let (a, b) = (trace[n] - BASE, trace[n + 1] - BASE);
            if a < 0.0 && b >= 0.0 {
                ups.push(n as f64 + (-a) / (b - a));
            }
        }
        assert!(ups.len() >= 5, "{} crossings", ups.len());
        let period = (ups[ups.len() - 1] - ups[0]) / (ups.len() - 1) as f64;
        let want = SR / hz;
        assert!((period - want).abs() < 0.5, "{period} vs {want}");
    }

    /// A rate that moves **lands at its grid point** and the phase is continuous across it:
    /// a retarget at frame 2500, four frames into a segment, leaves the segment it is in on
    /// the old rate and the next begins on the new, so the delay follows the exact sine of a
    /// phase that kept running, at the old rate to frame 2512 and at the new one after. Frame
    /// 2500 is near a zero crossing, where the delay moves fastest, so a phase that restarted,
    /// or a rate that arrives a segment early or late, is several frames off.
    #[test]
    fn a_rate_change_lands_at_its_grid_point_and_the_phase_stays_continuous() {
        let depth = 0.01f64;
        let (old, new, len) = (20.0f64, 30.0f64, 4_000);
        let (at, grid) = (2_500usize, 2_512usize);
        assert!(!at.is_multiple_of(COEFF_INTERVAL), "mid-segment");
        assert!(grid.is_multiple_of(COEFF_INTERVAL));
        let first = controls(old, depth, 0.0);
        let to = controls(new, depth, 0.0);
        let exact = exact_delays(len, depth, old, &[(grid, new)]);
        let bound = chord_bound(depth, new) + 0.05;
        let trace = trace(&mut trajectory(), &[(0, first), (at, to)], len);
        let worst = (FROM..len)
            .map(|n| (trace[n] - exact[n]).abs())
            .fold(0.0, f64::max);
        assert!(worst <= bound, "worst {worst} over {bound}");
        // Continuous: no frame of the render moves faster than the sweep's own greatest
        // slope at the new rate (`2 pi rate depth`, in frames a frame) and the chord's kink.
        let slope = std::f64::consts::TAU * new * depth;
        let jump = (FROM..len - 1)
            .map(|n| (trace[n + 1] - trace[n]).abs())
            .fold(0.0, f64::max);
        assert!(jump <= slope * 1.02 + 0.01, "jump {jump} past {slope}");
        // The old rate holds to the grid point.
        let on_old = exact_delays(len, depth, old, &[]);
        for n in at..grid {
            assert!(
                (trace[n] - on_old[n]).abs() <= chord_bound(depth, old) + 0.05,
                "frame {n} is not on the old rate"
            );
        }
        let sep = (grid..grid + 16)
            .map(|n| (on_old[n] - exact[n]).abs())
            .fold(0.0, f64::max);
        assert!(sep > 10.0 * bound, "the two rates are told apart: {sep}");
    }

    /// The rate is **floored at 0.01 Hz**, a control signal's value and a cell's alike: a rate below it,
    /// zero or negative, is the floor's step, and not a sweep that stands still or runs
    /// backwards. Over the first 4,800 frames (0.1 s) the delay has risen by `depth * sr *
    /// sin(2 pi 0.01 * 0.1)`, a figure no other rate gives. A rate that is not a number is
    /// the default's.
    #[test]
    fn a_rate_below_the_floor_is_the_floor() {
        let depth = 0.01f64;
        let (len, at) = (6_000, 4_800);
        let default_hz = ModDelayControls::DEFAULT.rate.get();
        for (hz, effective) in [
            (0.01f64, 0.01),
            (0.005, 0.01),
            (0.0, 0.01),
            (-3.0, 0.01),
            (-1e-9, 0.01),
            (f64::NEG_INFINITY, default_hz),
            (f64::NAN, default_hz),
        ] {
            let c = controls(hz, depth, 0.0);
            let trace = trace(&mut trajectory(), &[(0, c)], len);
            let rise = trace[at] - BASE;
            // The LFO steps before the frame it sounds in: frame `at` sounds the
            // phase `at + 1` steps on.
            let sounded = (at + 1) as f64;
            let figure = depth * SR * (std::f64::consts::TAU * effective * sounded / SR).sin();
            assert!(
                (rise - figure).abs() < 0.05,
                "rate {hz} rose {rise}, the {effective} Hz sweep {figure}"
            );
        }
    }

    /// A depth that moves is a pitch bend, so it ramps at no more than [`DELAY_SLEW`] frames
    /// of delay per frame, however far it moves, and **lands exactly**: a depth moved from
    /// 5 ms to 40 ms at 48 kHz is 1,680 frames of excursion at `1/8` a frame, 13,440 frames
    /// of ramp, and the sweep after it is the new depth to the frame. The delay's own motion
    /// (a slow 0.1 Hz sweep, at most `2 pi rate depth`) adds to the slew, and nothing else
    /// does.
    #[test]
    fn a_depth_ramp_is_bounded_by_the_slew_and_lands_exactly() {
        let (from, to, hz) = (0.005f64, 0.04f64, 0.1f64);
        // The ramp of 13,440 frames straddles the sweep's crest (the quarter period, frame
        // 120,000, where the LFO is at 1), so the excursion's growth is the delay's, a
        // frame a frame at its fastest.
        let (moved, len) = (113_280, 140_000);
        let trace = trace(
            &mut trajectory(),
            &[(0, controls(hz, from, 0.0)), (moved, controls(hz, to, 0.0))],
            len,
        );
        let steady = std::f64::consts::TAU * hz * to;
        let worst = (FROM..len - 1)
            .map(|n| (trace[n + 1] - trace[n]).abs())
            .fold(0.0, f64::max);
        let slew = DELAY_SLEW;
        assert!(
            worst <= slew + steady * 1.02 + 0.01,
            "the delay moved {worst} frames in one"
        );
        assert!(worst > slew * 0.9, "it ramps at the slew: {worst}");
        let arrived = moved + 13_440 + 64;
        let amp_at = |n: usize| (trace[n] - BASE) / (to * SR);
        let sine_at = |n: usize| (std::f64::consts::TAU * hz * n as f64 / SR).sin();
        for n in [arrived + 1_000, arrived + 6_000, len - 100] {
            let want = sine_at(n);
            assert!(
                (amp_at(n) - want).abs() < 2e-3,
                "frame {n}: {} of the new depth, the sine says {want}",
                amp_at(n)
            );
        }
        let half = moved + 6_720;
        let on_the_way = (trace[half] - BASE) / SR;
        assert!(
            (on_the_way - (from + (to - from) * 0.5) * sine_at(half)).abs() < 1e-4,
            "{on_the_way} halfway between the depths"
        );
    }

    /// Channel `c`'s LFO is `c` times the step ahead of channel 0's (a full cycle is no
    /// stagger), and the channels' lines are their own: a silent channel stays silent while
    /// another sounds, and a ramp through each shows its own delay, the exact sine of its
    /// phase offset at a control point.
    #[test]
    fn each_channel_is_staggered_by_its_step_and_has_a_line_of_its_own() {
        let c = controls(3.0, 0.005, 0.0);
        let len = 8_000;
        let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
        let mut k = prepared_kernel(
            ModDelayLayout::builder(bus(3), Sweep::CHORUS)
                .stagger(Stagger::of_step(PhaseIncrement::new(0.25)))
                .build()
                .unwrap(),
            RATE,
        );
        let y = drive_kernel(
            &mut k,
            &[(0, c)],
            &[ramp.clone(), vec![0.0; len], ramp.clone()],
        );
        assert!(
            y[1].iter().all(|&s| s == 0.0),
            "no bleed into a silent channel"
        );
        let inc = PhaseIncrement::per_sample(Hz::new(3.0), RATE);
        let mut phase = Phase::START;
        let (base, amp) = (0.01 * SR, 0.005 * SR);
        for (n, _) in ramp.iter().enumerate() {
            phase = phase.advance(inc);
            let turns = phase.get();
            if n < 600 || !(n + 1).is_multiple_of(COEFF_INTERVAL) {
                continue;
            }
            for (ch, offset) in [(0usize, 0.0), (2, 0.5)] {
                let want = base + amp * (std::f64::consts::TAU * (turns + offset)).sin();
                let got = n as f64 - f64::from(y[ch][n]);
                assert!(
                    (got - want).abs() < 0.05,
                    "channel {ch}, frame {n}: delay {got}, want {want}"
                );
            }
        }
        // A whole cycle of stagger is none.
        let mut k = prepared_kernel(
            ModDelayLayout::builder(bus(2), Sweep::CHORUS)
                .stagger(Stagger::of_step(PhaseIncrement::new(1.0)))
                .build()
                .unwrap(),
            RATE,
        );
        let both = drive_kernel(&mut k, &[(0, c)], &[ramp.clone(), ramp.clone()]);
        assert_eq!(both[0], both[1], "a full-cycle stagger is no stagger");
        // The offsets can be named: channel 1 half a cycle ahead, the two delays mirrored
        // about the base.
        let named =
            Stagger::try_from(&[PhaseIncrement::new(0.0), PhaseIncrement::new(0.5)][..]).unwrap();
        let mut k = prepared_kernel(
            ModDelayLayout::builder(bus(2), Sweep::CHORUS)
                .stagger(named)
                .build()
                .unwrap(),
            RATE,
        );
        let y = drive_kernel(&mut k, &[(0, c)], &[ramp.clone(), ramp]);
        let mid = 4_000;
        let d = |ch: usize| mid as f64 - f64::from(y[ch][mid]);
        assert!(
            (d(0) + d(1) - 2.0 * base).abs() < 0.2,
            "{} and {} around {base}",
            d(0),
            d(1)
        );
    }

    /// Channel 3 of a 6-wide chorus (quarter-cycle stagger) is a cycle from channel 4's and
    /// 4 quarter cycles from channel 0's, so channel 4 is channel 0's sweep to the bit; a
    /// silent channel stays silent whatever its neighbours do, and a half-cycle stagger
    /// differs.
    #[test]
    fn six_channels_are_six_swept_lines() {
        let mut node = prepared(
            ModDelayNode::<f32>::from(ModDelayConfig::chorus(bus(6))),
            RATE,
            128,
        );
        assert!(node.param_set().set(UnitParam::Feedback, 0.0));
        let len = 4_096usize;
        let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
        let silent = vec![0.0f32; len];
        let ins: [&[f32]; 6] = [&ramp, &ramp, &ramp, &silent, &ramp, &ramp];
        let mut out = vec![Vec::new(); 6];
        for at in (0..len).step_by(128) {
            let chunk: Vec<&[f32]> = ins.iter().map(|c| &c[at..at + 128]).collect();
            for (o, b) in
                out.iter_mut()
                    .zip(drive(&mut node, RATE, Planes::new(&chunk).unwrap(), &[]))
            {
                o.extend(b);
            }
        }
        assert!(
            out[3].iter().all(|&s| s == 0.0),
            "no bleed into a silent channel"
        );
        assert_eq!(out[4], out[0], "a full-cycle stagger is no stagger");
        assert_ne!(out[2], out[0], "a half-cycle stagger differs");
        assert_ne!(out[1], out[0], "a quarter-cycle stagger differs");
    }

    /// A re-prepare restarts the sweep: the LFO's phase is zero again and no segment is
    /// solved, so the delay is the exact sine from the start, as after a reset. The lines are
    /// emptied too: the rate is new, and the delays in frames were the old one's.
    #[test]
    fn a_re_prepare_restarts_the_sweep() {
        let (depth, hz) = (0.01f64, 20.0f64);
        let c = controls(hz, depth, 0.0);
        let mut k = trajectory();
        drive_kernel(&mut k, &[(0, c)], &[vec![1.0; 1_003]]);
        assert_ne!(
            k.clock.state().0,
            RunningPhase::START,
            "the sweep was running"
        );
        k.prepare(RATE, MaxBlock::of(Samples::new(64)));
        assert_eq!(k.clock.state(), (RunningPhase::START, 0, false, true));
        let len = 6_000;
        let trace = trace(&mut k, &[(0, c)], len);
        let exact = exact_delays(len, depth, hz, &[]);
        for n in (FROM..len).filter(|n| (n + 1).is_multiple_of(COEFF_INTERVAL)) {
            assert!(
                (trace[n] - exact[n]).abs() < 0.05,
                "frame {n}: {} vs {}",
                trace[n],
                exact[n]
            );
        }
    }

    /// A reset restores the sweep's start and empties the lines: the same stream rendered
    /// after a reset, at the same controls, is a fresh node's bit for bit, though the reset
    /// came at a frame that is not a grid point.
    #[test]
    fn a_reset_node_renders_what_a_fresh_one_does() {
        let x: Vec<f32> = (0..400)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let c = ModDelayControls {
            rate: Hz::new(7.0),
            depth: Seconds::new(0.004),
            feedback: Feedback::new(0.6),
        };
        let blocks = |node: &mut ModDelayNode<f32>, from: usize| -> Vec<f32> {
            let mut out = Vec::new();
            for chunk in x[from..].chunks(37) {
                out.extend(drive(node, RATE, Planes::new(&[chunk, chunk]).unwrap(), &[]).remove(0));
            }
            out
        };
        let make = || {
            prepared(
                ModDelayNode::<f32>::from(
                    ModDelayConfig::new(
                        ModDelayLayout::builder(ChannelTopology::STEREO, Sweep::CHORUS)
                            .build()
                            .unwrap(),
                    )
                    .controls(c),
                ),
                RATE,
                64,
            )
        };
        let mut used = make();
        blocks(&mut used, 100);
        used.reset();
        let mut fresh = make();
        assert_eq!(blocks(&mut used, 0), blocks(&mut fresh, 0));
    }

    /// A cell that moves **between** grid points starts at the next grid point: a rate, a
    /// depth and a feedback written 13 frames past frame 1024 and heard from the block that
    /// begins there render the same as the same write at frame 1040, the next grid point, and
    /// differently from the write at 1056.
    #[test]
    fn a_move_between_grid_points_starts_at_the_next_one() {
        let len = 2_048;
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |at: usize| -> Vec<f32> {
            let node = ModDelayNode::<f32>::from(ModDelayConfig::chorus(ChannelTopology::MONO));
            let set = node.param_set();
            let mut node = prepared(node, RATE, 2_048);
            let mut out = Vec::new();
            for (chunk, from) in [(&x[..at], 0), (&x[at..], at)] {
                if from == at {
                    assert!(set.set(UnitParam::Rate, 8.0));
                    assert!(set.set(UnitParam::Depth, 0.002));
                    assert!(set.set(UnitParam::Feedback, 0.6));
                }
                out.extend(drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &[]).remove(0));
            }
            out
        };
        let at_the_point = render(1_040);
        assert_eq!(render(1_037), at_the_point);
        assert_ne!(render(1_056), at_the_point);
    }

    /// A clone shares no cell and no line: it starts silent at the sweep's start, and
    /// writing to one's cells leaves the other's unmoved.
    #[test]
    fn a_clone_shares_nothing() {
        let a = ModDelayNode::<f32>::from(ModDelayConfig::flanger(ChannelTopology::MONO));
        let set = a.param_set();
        let loud: Vec<f32> = (0..256).map(|n| ((n * 7) % 13) as f32 - 6.0).collect();
        let mut a = prepared(a, RATE, 64);
        for chunk in loud.chunks(64) {
            drive(&mut a, RATE, Planes::new(&[chunk]).unwrap(), &[]);
        }
        let mut b = a.clone();
        let silence = vec![0.0f32; 64];
        let out = drive(&mut b, RATE, Planes::new(&[&silence]).unwrap(), &[]);
        assert!(out[0].iter().all(|&s| s == 0.0), "a clone starts silent");
        let out = drive(&mut a, RATE, Planes::new(&[&silence]).unwrap(), &[]);
        assert!(out[0].iter().any(|&s| s != 0.0), "the original rings on");
        // The cells are shared by a clone (it is the same param set) but a fork is not.
        let fork = a.fork_fresh();
        assert!(set.set(UnitParam::Feedback, 0.1));
        assert_eq!(fork.param_set().get(UnitParam::Feedback), Some(0.7));
    }

    /// The rate is **floored at 0.01 Hz, a control signal's value and a cell's alike**: a rate of
    /// -3 Hz, written to the cell or fed on the port, is the 0.01 Hz sweep, which has raised
    /// the 10 ms base by `5 ms * sin(2 pi 0.01 * 0.1 s)`, 1.508 frames, after 0.1 s.
    #[test]
    fn a_rate_below_the_floor_is_the_floor_in_a_cell_and_on_a_signal() {
        let figure = 0.005 * SR * (std::f64::consts::TAU * 0.01 * 0.1).sin();
        let at = 4_800;
        let delays = |node: ModDelayNode<f32>, signal: Option<f32>| -> Vec<f64> {
            let mut node = prepared(node, RATE, 64);
            let x: Vec<f32> = (0..at + 64).map(|n| n as f32).collect();
            let mut y = Vec::new();
            for chunk in x.chunks(64) {
                let rate = signal.map(|v| vec![v; chunk.len()]);
                let params = [rate.as_deref(), None, None];
                y.extend(drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &params).remove(0));
            }
            y.iter()
                .enumerate()
                .map(|(n, &v)| n as f64 - f64::from(v))
                .collect()
        };
        let chorus = || {
            let node = ModDelayNode::<f32>::from(ModDelayConfig::chorus(ChannelTopology::MONO));
            assert!(node.param_set().set(UnitParam::Feedback, 0.0));
            node
        };
        let on_cell = {
            let node = chorus();
            assert!(node.param_set().set(UnitParam::Rate, -3.0));
            delays(node, None)
        };
        let on_signal = delays(chorus(), Some(-3.0));
        for (what, d) in [("cell", on_cell), ("signal", on_signal)] {
            assert!(
                (d[at] - (480.0 + figure)).abs() < 0.05,
                "a negative rate on a {what}: delay {}, the 0.01 Hz sweep {}",
                d[at],
                480.0 + figure
            );
        }
    }

    /// A control the graph drives is sampled on the stream's 16-frame grid, not at the
    /// block's last frame, so what is heard does not depend on how the stream is cut into
    /// blocks; and the control signal is heard, since the cell it overrides is a different depth.
    #[test]
    fn a_driven_slow_control_is_sampled_on_the_grid_whatever_the_blocks() {
        let len = 4_096;
        let signal: Vec<f32> = (0..len)
            .map(|n| 0.001 + 0.009 * n as f32 / len as f32)
            .collect();
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |block: usize, driven: bool| -> Vec<f32> {
            let mut node = prepared(
                ModDelayNode::<f32>::from(ModDelayConfig::chorus(ChannelTopology::MONO)),
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

    /// A feedback moved at a control signal corner **off the grid** aims its ramp at once,
    /// and the delays wait for the next corner: after 613 frames (frame 5 of a segment) the
    /// retarget puts the feedback's target on the new value, and every channel's segment ramp
    /// is the one in flight.
    #[test]
    fn a_feedback_moved_off_the_grid_aims_at_once_and_the_delays_wait_for_the_corner() {
        let x: Vec<Vec<f32>> = vec![
            (0..613)
                .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
                .collect(),
        ];
        let (first, second) = (controls(2.0, 0.003, 0.1), controls(2.0, 0.003, 0.9));
        let mut k = prepared_kernel(ModDelayLayout::chorus(bus(1)), RATE);
        drive_kernel(&mut k, &[(0, first)], &x);
        assert_eq!(k.clock.state().1, 613 % COEFF_INTERVAL);
        let segment = (k.clock.state().0, k.delays[0].target());
        k.retarget(&second);
        assert_eq!(k.feedback.target(), f32::from_unit(Feedback::new(0.9)));
        assert_eq!((k.clock.state().0, k.delays[0].target()), segment);
    }

    /// A fork taken **mid-segment** lands its first design: a clone of a node that has rendered
    /// 613 frames (frame 5 of a segment), its depth cell moved from 2 ms to 10 ms, renders the
    /// exact sweep at 10 ms from its first solved segment. A depth that slewed in from the old
    /// 2 ms at [`DELAY_SLEW`] frames a frame (3,072 frames for the 384 between them) would be
    /// short of it at frame 1,700.
    #[test]
    fn a_fork_taken_mid_segment_lands_its_first_design() {
        let (hz, depth) = (20.0f64, 0.01f64);
        let layout = trajectory_layout(1, Stagger::NONE);
        let node = ModDelayNode::<f32>::from(
            ModDelayConfig::new(layout).controls(controls(hz, 0.002, 0.0)),
        );
        let set = node.param_set();
        let mut node = prepared(node, RATE, 64);
        let ramp: Vec<f32> = (0..613).map(|n| n as f32).collect();
        for chunk in ramp.chunks(64) {
            drive(&mut node, RATE, Planes::new(&[chunk]).unwrap(), &[]);
        }
        let mut fork = node.clone();
        assert!(set.set(UnitParam::Depth, depth));
        let len = 1_800;
        let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
        let out: Vec<f32> = ramp
            .chunks(64)
            .flat_map(|c| drive(&mut fork, RATE, Planes::new(&[c]).unwrap(), &[]).remove(0))
            .collect();
        let exact = exact_delays(len, depth, hz, &[]);
        let bound = chord_bound(depth, hz) + 0.05;
        for n in (FROM..len).filter(|n| (n + 1).is_multiple_of(COEFF_INTERVAL)) {
            let got = n as f64 - f64::from(out[n]);
            assert!(
                (got - exact[n]).abs() < bound,
                "frame {n}: {got} vs {}",
                exact[n]
            );
        }
    }

    /// A reset node equals a fresh one that has the cell: render, move a cell, reset, and the
    /// render is the fresh node's with that cell (the move lands at once, it does not ramp
    /// from the old value).
    #[test]
    fn a_cell_moved_before_a_reset_lands_at_once_as_on_a_fresh_node() {
        let x: Vec<f32> = (0..1_000)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let moved = |node: &ModDelayNode<f32>| {
            let set = node.param_set();
            assert!(set.set(UnitParam::Feedback, 0.9));
            assert!(set.set(UnitParam::Depth, 0.004));
            assert!(set.set(UnitParam::Rate, 5.0));
        };
        let blocks = |node: &mut ModDelayNode<f32>| -> Vec<f32> {
            x.chunks(64)
                .flat_map(|c| drive(node, RATE, Planes::new(&[c]).unwrap(), &[]).remove(0))
                .collect()
        };
        let mut used = prepared(
            ModDelayNode::<f32>::from(ModDelayConfig::chorus(ChannelTopology::MONO)),
            RATE,
            64,
        );
        blocks(&mut used);
        moved(&used);
        used.reset();
        let fresh = ModDelayNode::<f32>::from(ModDelayConfig::chorus(ChannelTopology::MONO));
        moved(&fresh);
        let mut fresh = prepared(fresh, RATE, 64);
        assert_eq!(blocks(&mut used), blocks(&mut fresh));
    }

    // ---- the vibrato preset ------------------------------------------------------------

    /// A vibrato reads its input at the rate `1 - d'(t)` of its delay `d`, so a ramp through it
    /// comes out with the slope `1 - d'`: the peak rise of the slope is `2 pi r D` over 1, with
    /// the sweep's amplitude `D = (2^(c / 1200) - 1) / (2 pi r)` the closed form of a depth of
    /// `c` cents at rate `r`, and the peak fall is the same step below 1. The delay is the
    /// corners' chords, whose slope is `sin(8 h) / (8 h)` of the sine's at best and loses
    /// `cos` of the angle between a segment's middle and the peak, at most `8 h` for `h` radians a frame:
    /// together under `(8 h)^2` of the peak. Measured over whole cycles on an `f64` ramp, which
    /// the linear read keeps exact.
    #[test]
    fn a_vibratos_peak_pitch_swing_is_its_depth_in_cents() {
        let len = 96_000;
        for (cents, hz) in [(50.0, 5.0), (100.0, 2.0), (200.0, 8.0)] {
            let mut k = ModDelayKernel::<f64>::new(ModDelayLayout::vibrato(ChannelTopology::MONO));
            k.prepare(RATE, MaxBlock::of(Samples::new(len)));
            let controls = Sweep::VIBRATO
                .vibrato_controls(Cents::new(cents), ModulationRate::of(Hz::new(hz)))
                .unwrap();
            assert_eq!(controls.feedback, Feedback::NONE);
            let x: Vec<f64> = (0..len).map(|n| n as f64).collect();
            let y = drive_kernel(&mut k, &[(0, controls)], &[x]).remove(0);
            let slopes: Vec<f64> = y.windows(2).skip(2_000).map(|w| w[1] - w[0]).collect();
            let swing = Cents::new(cents).to_pitch_ratio() - 1.0;
            let h = std::f64::consts::TAU * hz / SR;
            let rise = slopes.iter().copied().fold(f64::MIN, f64::max) - 1.0;
            let fall = 1.0 - slopes.iter().copied().fold(f64::MAX, f64::min);
            for (name, got) in [("rise", rise), ("fall", fall)] {
                assert!(
                    got <= swing * (1.0 + 1e-9),
                    "{cents} cents at {hz} Hz, {name} {got}"
                );
                assert!(
                    got >= swing * (1.0 - (8.0 * h).powi(2)),
                    "{cents} cents at {hz} Hz, {name} {got} vs {swing}"
                );
            }
        }
    }

    /// The preset is wet-only with no feedback (so a finite tail of its 25 ms line), no latency,
    /// every channel's sweep in step, and starts at 5 Hz and 50 cents.
    #[test]
    fn the_vibrato_preset_has_no_feedback_and_a_finite_tail() {
        let node = prepared(
            ModDelayNode::<f32>::from(ModDelayConfig::vibrato(ChannelTopology::STEREO)),
            RATE,
            64,
        );
        assert_eq!(node.shape().tail, Tail::Finite(Samples::new(1_200)));
        assert_eq!(node.shape().latency, Latency::new(Samples::new(0)));
        assert_eq!(node.param_base(0), Some(5.0));
        let depth = (Cents::new(50.0).to_pitch_ratio() - 1.0) / (std::f64::consts::TAU * 5.0);
        assert!((node.param_base(1).unwrap() - depth).abs() < 1e-15);
        assert_eq!(node.param_base(2), Some(0.0));
        assert_eq!(
            ModDelayLayout::vibrato(ChannelTopology::STEREO).offsets()[1],
            PhaseIncrement::new(0.0)
        );
    }
}
