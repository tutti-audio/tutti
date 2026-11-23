//! State-variable filter — the crate's general-purpose filter.
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change. One width-generic node,
//! [`SvfFilterNode`]: the coefficients are shared across channels and only the
//! integrator state is per channel. The algorithm is `tutti-dsp`'s
//! ([`SvfBank`]); the node is a [`KernelNode`] over it, and this file holds the
//! kernel, the cells and how a block reads them.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, Ramp, SimdLanes, SvfBank, SvfCoefficients, SvfControls, SvfModel, SvfType,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, Hz, Q, SampleRate, Tail};

use super::filter_bus;
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// A state-variable filter of any width: `N` audio inputs, `N` outputs, one
/// coefficient set shared across the channels and one integrator pair per
/// channel.
///
/// Eight responses ([`SvfType`]) share one coefficient solve (Cytomic /
/// Zavalishin TPT form), so switching type is as cheap as any parameter
/// change. The response is fixed at construction: a node with another one is
/// another node. Made from an [`SvfFilterConfig`] (`SvfFilterNode::from`).
///
/// # Examples
///
/// ```
/// # use tutti_buffers::Planes;
/// use tutti_buffers::{ChannelLayout, MaxBlock};
/// use tutti_units::{Hz, Q, UnitParam};
/// use tutti_graph::{Prepare, Renderer};
/// use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
/// use tutti_units::{SampleRate, Samples};
///
/// let config = SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
///     .freq(Hz::new(800.0))
///     .q(Q::new(0.707));
/// let filter = SvfFilterNode::<f32>::from(config);
/// let (mut renderer, _, controls) = Renderer::alone(filter, Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
/// let out = renderer.render_input(Planes::new(&[&[1.0; 64]]).unwrap());
/// assert_eq!(out.len(), 1);
///
/// // Move the cutoff on the running node; it ramps over the next block.
/// assert!(controls.set(UnitParam::Cutoff, 2_000.0));
/// ```
///
/// Cutoff ([`Hz`]), [`Q`] and gain ([`Db`]) are live [`Params`](crate::Params) shared across
/// clones. A held value costs nothing: the model is designed again only when one
/// moves past a small dead-band ([`SvfControls::differs_from`]). The filter is
/// designed on a 16-frame grid counted from the last reset, at each segment's
/// first frame, and the coefficients ramp linearly to that solve over the segment,
/// so an automated cutoff ramps rather than stepping, arrives within 16 frames
/// and renders the same however the stream is cut into blocks. A cell that moved
/// ramps from the last solve to its value across the block, so it is the one
/// control whose render depends on the block's length.
///
/// Gain applies only to [`Bell`](SvfType::Bell),
/// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf); the
/// other types ignore it.
///
/// `F` is the float of the graph it runs in, and of the integrators and the coefficients. A
/// filter at a low cutoff in an `f32` graph keeps the relative precision its topology gives it
/// (its documented range); an `f64` graph widens that.
///
/// # Modulated params
///
/// Cutoff (in [`Hz`]), [`Q`] and the gain (in [`Db`]) are modulatable by the
/// graph, in that port order. A per-frame value on a param port
/// ([`Io::param`](tutti_graph::Io::param)) **overrides** the corresponding cell
/// (and a control signal on any of the three stops the cells ramping): it is sampled at the
/// first frame of each 16-frame segment, with the coefficients interpolated over
/// the segment, instead of a `tan` per sample. Unmodulated, the filter reads its
/// cells once per block at the cost of one branch, which is the common case.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, its controls are
/// a [`ParamSet`](tutti_graph::ParamSet) over cutoff, Q and gain by [`UnitParam`](tutti_units::UnitParam),
/// and a fork of it starts from the values last set through that set. Its tail is
/// [`Tail::Unknown`]: a resonant filter rings on after
/// its input stops, for as long as its Q says, so the graph's tail fold must not
/// read it as `None`; it never parks. The graph prepares it at the device rate
/// before its first block. Rendering allocates nothing.
///
/// # Panics
///
/// `SvfFilterNode::from` panics past 16 channels: a filter is one bus.
pub type SvfFilterNode<F> = KernelNode<SvfKernel<F>>;

/// What an [`SvfFilterNode`] is made from: its width and its controls' start.
///
/// A config starts at a 1 kHz Butterworth response of the given type with 0 dB gain
/// ([`SvfControls::DEFAULT`]); the options set the cutoff, the resonance and the gain. The
/// response is fixed: a node with another one is another node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfFilterConfig {
    width: ChannelLayout,
    controls: SvfControls,
}

impl SvfFilterConfig {
    /// A filter of `width` channels with the response `filter_type`.
    ///
    /// The coefficients are channel-shared, one linked control surface across all
    /// channels, and only the per-channel integrator state is replicated, so widening costs
    /// state but not parameter work. Speaker placement is the upstream panner's job: this
    /// is a per-channel filter, not a spatial process.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, filter_type: SvfType) -> Self {
        Self {
            width: width.into(),
            controls: SvfControls {
                ty: filter_type,
                ..SvfControls::DEFAULT
            },
        }
    }

    /// The cutoff, centre or corner, at the start.
    ///
    /// The coefficients are solved at the rate [`prepare`](tutti_graph::Node::prepare)
    /// hands the node.
    #[must_use]
    pub fn freq(mut self, frequency: Hz) -> Self {
        self.controls.freq = frequency;
        self
    }

    /// The resonance at the start. Around `0.707` is the flattest (Butterworth) response;
    /// higher values resonate at the cutoff, and a band-pass or notch narrows as it rises.
    #[must_use]
    pub fn q(mut self, q: Q) -> Self {
        self.controls.q = q;
        self
    }

    /// The gain of a bell or shelf at the start, read only by [`Bell`](SvfType::Bell),
    /// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf).
    #[must_use]
    pub fn gain(mut self, gain: Db) -> Self {
        self.controls.gain = gain;
        self
    }
}

impl<F: Float + SimdLanes> From<SvfFilterConfig> for SvfFilterNode<F> {
    fn from(config: SvfFilterConfig) -> Self {
        Self::with_kernel(
            SvfKernel::new(filter_bus(config.width)),
            SvfSet(config.controls.ty),
            config.controls,
        )
    }
}

/// The steps the coefficients take to a new design: one grid period, so a design
/// arrives at the next grid point.
const RAMP_STEPS: u32 = 16;

/// The state-variable filter as a [`Kernel`]: an [`SvfBank`] in the float `F`
/// and the ramp of the coefficients every channel shares.
#[derive(Clone, Debug)]
pub struct SvfKernel<F: Float + SimdLanes> {
    input: BusTopology,
    bank: SvfBank<F>,
    /// The coefficients in flight; the first design after `prepare` lands at once.
    coefficients: Ramp<SvfCoefficients<F>>,
    rate: SampleRate,
    applied: bool,
}

impl<F: Float + SimdLanes> SvfKernel<F> {
    /// A filter on the bus `input`, its output where it is. Allocates the state. Its
    /// coefficients until `prepare` are a placeholder at 48 kHz: an unprepared node renders
    /// nothing, and `prepare` designs for the rate it is given.
    fn new(input: BusTopology) -> Self {
        let rate = SampleRate::SR_48K;
        Self {
            input,
            bank: SvfBank::new(input.width()),
            coefficients: Ramp::new(Self::design(&SvfControls::DEFAULT, rate)),
            rate,
            applied: false,
        }
    }

    fn design(controls: &SvfControls, rate: SampleRate) -> SvfCoefficients<F> {
        SvfCoefficients::from(&SvfModel::new(*controls, rate))
    }
}

impl<F: Float + SimdLanes> Kernel for SvfKernel<F> {
    type Float = F;
    type Set = SvfSet;

    // A design arrives at the next grid point: the ramp is one grid period long.
    const GRID: usize = RAMP_STEPS as usize;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.coefficients = Ramp::new(Self::design(&SvfControls::DEFAULT, rate)).steps(RAMP_STEPS);
        self.applied = false;
    }

    fn reset(&mut self) {
        self.coefficients.jump_to_target();
        self.bank.reset();
    }

    fn retarget(&mut self, controls: &SvfControls) {
        self.coefficients
            .retarget(Self::design(controls, self.rate));
        if !self.applied {
            self.coefficients.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.bank.process(&mut self.coefficients, block)
    }

    /// A resonant filter rings for as long as its Q says, which is too dear to find per
    /// block: unknown, so the graph's tail fold never reads it as none.
    fn tail(&self) -> Tail {
        Tail::Unknown
    }
}

/// The controls of an [`SvfFilterNode`]: cutoff, Q and gain, with the response
/// they are for.
#[derive(Clone, Copy, Debug)]
pub struct SvfSet(SvfType);

control_set! {
    SvfSet(s): SvfControls = SvfControls { ty: s.0, ..SvfControls::DEFAULT };
    live: (all);
    Cutoff => Hz = freq,
    Q => Q = q,
    GainDb => Db = gain;

    fn moved(&self, applied: &SvfControls, target: &SvfControls) -> bool {
        target.differs_from(applied)
    }

    fn defaults(&self) -> SvfControls {
        SvfControls::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use tutti_buffers::Planes;
    // These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.
    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use crate::test_support::{RATE, drive_block, prepared};
    use tutti_graph::contract::{assert_param_fork, drive};
    use tutti_graph::{Node, ParamNode};
    use tutti_units::SampleRate;
    use tutti_units::UnitParam;

    /// `node`, prepared at 44.1 kHz, over a mono `input` in one block.
    fn render(node: SvfFilterNode<f32>, input: &[f32]) -> Vec<f32> {
        let rate = SampleRate::SR_44K1;
        let mut node = tutti_graph::contract::prepared(node, rate, input.len());
        drive(&mut node, rate, Planes::new(&[input]).unwrap(), &[]).remove(0)
    }

    /// `node`, prepared at 44.1 kHz, over `inputs` (one slice per channel)
    /// in one block.
    fn render_wide(node: SvfFilterNode<f32>, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
        let rate = SampleRate::SR_44K1;
        let mut node = tutti_graph::contract::prepared(node, rate, inputs[0].len());
        drive(&mut node, rate, Planes::new(inputs).unwrap(), &[])
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it. See
    /// `tutti_graph::contract::assert_param_fork` for what it pins.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::Bell)
                .freq(Hz::new(500.0))
                .q(Q::new(0.707))
                .gain(Db::new(-3.0)),
        );
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb]
        );
        assert_param_fork(node);
    }

    /// The by-address path: a `UnitParam` set through the node's
    /// `ParamSet` lands in the same cell the bespoke setter writes, and an
    /// address the node does not own is refused.
    #[test]
    fn a_unit_param_lands_in_the_cell_and_a_foreign_address_is_refused() {
        let node = SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::LowPass)
                .freq(Hz::new(1000.0))
                .q(Q::new(0.707)),
        );
        let params = node.param_set();
        assert!(params.set(UnitParam::Cutoff, 5000.0));
        assert!(params.set(UnitParam::Q, 2.5));
        assert_eq!(node.param_base(0), Some(5000.0));
        assert_eq!(node.param_base(1), Some(2.5));
        assert!(!params.set(UnitParam::Wet, 0.5));
    }

    #[test]
    fn each_stereo_channel_is_the_mono_filter_of_its_input() {
        // Identical signal on L+R must match what a mono SVF would produce.
        let signal = generate_sine(440.0, 44100.0, 2048);
        let mono_out = render(
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                    .freq(Hz::new(1000.0))
                    .q(Q::new(0.707)),
            ),
            &signal,
        );
        let out = render_wide(
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::LowPass)
                    .freq(Hz::new(1000.0))
                    .q(Q::new(0.707)),
            ),
            &[&signal, &signal],
        );
        for i in 0..signal.len() {
            assert!(
                (mono_out[i] - out[0][i]).abs() < 1e-5,
                "L channel diverges from mono at sample {i}"
            );
            assert!(
                (mono_out[i] - out[1][i]).abs() < 1e-5,
                "R channel diverges from mono at sample {i}"
            );
        }
    }

    #[test]
    fn wide_channel_matches_mono_and_is_independent() {
        // Each of 6 channels must filter exactly like a mono SVF with the same
        // coeffs, and carry only its own input (no cross-channel bleed).
        let sig = generate_sine(300.0, 44100.0, 2048);
        let silent = vec![0.0f32; sig.len()];
        let mut frames: Vec<&[f32]> = vec![&silent; 6];
        frames[4] = &sig;
        let out = render_wide(
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(6u16, SvfType::LowPass)
                    .freq(Hz::new(1000.0))
                    .q(Q::new(0.707)),
            ),
            &frames,
        );
        let mono_out = render(
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                    .freq(Hz::new(1000.0))
                    .q(Q::new(0.707)),
            ),
            &sig,
        );
        for i in 0..sig.len() {
            assert!(
                (mono_out[i] - out[4][i]).abs() < 1e-5,
                "ch4 diverges from mono at {i}"
            );
        }
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
    }

    // ── Modulated params (the graph's param ports) ──────────────────────────

    #[test]
    fn stereo_svf_fed_cutoff_modulates_response() {
        // Same noise through a low-pass: a cutoff fed high should pass more
        // energy than the same node fed low. Proves the param port actually
        // drives the coefficients.
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let run = |cutoff: f32| -> f32 {
            let mut f = prepared(SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::LowPass)
                    .freq(Hz::new(200.0))
                    .q(Q::new(0.707)),
            ));
            let held = vec![cutoff; noise.len()];
            let out = drive(
                &mut f,
                RATE,
                Planes::new(&[&noise, &noise]).unwrap(),
                &[Some(&held), None],
            );
            rms(&out[0][256..])
        };
        let low = run(200.0);
        let high = run(8000.0);
        assert!(
            high > low * 1.5,
            "higher fed cutoff should pass more: low={low}, high={high}"
        );
    }

    #[test]
    fn stereo_svf_unmodulated_process_matches_modulated_held_constant() {
        // A node whose fed cutoff is held at the same value as an unmodulated
        // node's cell must produce the same output — the modulated path is a
        // faithful superset.
        let signal = generate_sine(440.0, 48_000.0, 1024);
        let make = || {
            prepared(SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::LowPass)
                    .freq(Hz::new(1000.0))
                    .q(Q::new(0.707)),
            ))
        };
        let plain = drive_block(&mut make(), &[&signal, &signal]);
        let held = vec![1000.0f32; signal.len()];
        let modn = drive(
            &mut make(),
            RATE,
            Planes::new(&[&signal, &signal]).unwrap(),
            &[Some(&held), None],
        );
        for i in 0..signal.len() {
            assert!(
                (plain[0][i] - modn[0][i]).abs() < 1e-5,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain[0][i],
                modn[0][i]
            );
        }
    }

    // ── Per-block reads ──────────────────────────────────────────────────────

    /// A cutoff written between blocks ramps on the stream clock: the render is the
    /// same at every block length, the ramp is still under way one 64-frame block in
    /// (a design between the two cutoffs), and it has arrived [`GAIN_RAMP`] after the
    /// block it was first seen in (5 ms, 240 frames at 48 kHz; 512 frames are rendered).
    ///
    /// (A write *during* a block cannot be staged in a single-threaded test —
    /// the control thread writes between callbacks — so "between blocks" is the
    /// observable form of "not before the next block".)
    ///
    /// [`GAIN_RAMP`]: tutti_dsp::GAIN_RAMP
    #[test]
    fn a_cutoff_written_between_blocks_ramps_on_the_stream() {
        use crate::test_support::{assert_cell_ramps, render_change, sine};
        // 1.5 kHz: two whole periods in a 64-frame block, so a block's RMS is exact.
        let x = sine(1_500.0, 48_000.0, 64 + 512);
        let make = || {
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::LowPass)
                    .freq(Hz::new(400.0))
                    .q(Q::new(0.707)),
            )
        };
        let change = |n: &SvfFilterNode<f32>| {
            assert!(n.param_set().set(UnitParam::Cutoff, 6_000.0));
        };
        let (before, after) = ([&x[..64], &x[..64]], [&x[64..], &x[64..]]);
        let mut node = assert_cell_ramps(make, change, &before, &after, "svf cutoff");
        // Arrived: a block later the tone is at the 6 kHz low-pass's gain, about 1.
        let arrived = rms(&drive_block(&mut node, &before)[0]);
        let mut start = prepared(make());
        drive_block(&mut start, &before);
        let low = rms(&drive_block(&mut start, &before)[0]);
        let one = [&x[64..128], &x[64..128]];
        let (_, mid) = render_change(make, change, &before, &one, 64);
        let mid = rms(&mid[0]);
        assert!(
            (arrived - lp_rms(1_500.0, 6_000.0, 0.707)).abs() < 0.02 * arrived,
            "arrived {arrived}"
        );
        assert!(
            (low - lp_rms(1_500.0, 400.0, 0.707)).abs() < 0.01,
            "before {low}"
        );
        assert!(
            low < mid && mid < 0.95 * arrived,
            "under way after 64 frames: {low} < {mid} < {arrived}"
        );
    }

    /// The RMS of a unit sine at `hz` through a low-pass at `fc` and `q`, 48 kHz: the
    /// bilinear response `1 / sqrt((1 - u^2)^2 + (u / q)^2)`, `u = tan(pi hz / fs) / tan(pi fc / fs)`.
    fn lp_rms(hz: f64, fc: f64, q: f64) -> f32 {
        let u = libm::tan(core::f64::consts::PI * hz / 48_000.0)
            / libm::tan(core::f64::consts::PI * fc / 48_000.0);
        let gain = 1.0 / libm::sqrt((1.0 - u * u).powi(2) + (u / q).powi(2));
        (gain * core::f64::consts::FRAC_1_SQRT_2) as f32
    }

    /// A clone starts with its cells at rest: a clone of a node whose cutoff ramp is in
    /// flight renders its first block at the cell's cutoff, not at where the source's ramp
    /// stands on the source's clock.
    #[test]
    fn a_clone_of_a_node_mid_ramp_starts_at_rest() {
        use crate::test_support::sine;
        let rate = SampleRate::SR_48K;
        let mut node = tutti_graph::contract::prepared(
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                    .freq(Hz::new(400.0))
                    .q(Q::new(0.707)),
            ),
            rate,
            64,
        );
        let x = sine(1_500.0, 48_000.0, 64);
        let block = |n: &mut SvfFilterNode<f32>| {
            rms(&drive(n, rate, Planes::new(&[&x]).unwrap(), &[]).remove(0))
        };
        block(&mut node);
        assert!(node.param_set().set(UnitParam::Cutoff, 6_000.0));
        let source = block(&mut node);
        let arrived = lp_rms(1_500.0, 6_000.0, 0.707);
        assert!(
            source < 0.8 * arrived,
            "the source is under way: {source} vs {arrived}"
        );
        let mut clone = node.clone();
        let first = block(&mut clone);
        assert!(
            first > 0.85 * arrived,
            "the clone arrived: {first} vs {arrived}"
        );
    }

    /// A NaN Q at construction is read as the Butterworth Q, not as 0, which
    /// is below the type's floor. A low-pass at 800 Hz on an 800 Hz sine has a
    /// steady-state gain of Q at the cutoff, so RMS is `0.7071 · 0.7071 = 0.5` (0.457 measured: the bilinear warp costs a few percent);
    /// a later finite Q takes effect, and `reset` keeps the output finite.
    #[test]
    fn a_nan_q_at_construction_is_butterworth_and_recovers() {
        let x = generate_sine(800.0, 44_100.0, 8 * 1024);
        let mut f = prepared(SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(800.0))
                .q(Q::new(f64::NAN)),
        ));
        let mut last = Vec::new();
        for block in x.chunks(1024) {
            last = drive_block(&mut f, &[block]).remove(0);
            assert!(last.iter().all(|s| s.is_finite()), "NaN Q");
        }
        let level = rms(&last);
        assert!((level - 0.5).abs() < 0.05, "RMS {level}");
        assert!(f.param_set().set(UnitParam::Q, 2.0));
        for block in x.chunks(1024) {
            last = drive_block(&mut f, &[block]).remove(0);
            assert!(last.iter().all(|s| s.is_finite()), "after a Q write");
        }
        let raised = rms(&last);
        assert!(raised > 0.9, "Q 2 takes effect: {raised}");
        Node::reset(&mut f);
        let out = drive_block(&mut f, &[&x[..1024]]);
        assert!(out[0].iter().all(|s| s.is_finite()), "after reset");
    }

    /// A NaN in a raw cell never reaches the filter state — not even in the
    /// block where another control moves and forces a re-solve, which is the
    /// case a threshold comparison alone lets through. The filter keeps
    /// running at the last good cutoff, and picks up the next finite write.
    #[test]
    fn a_nan_in_a_raw_cell_never_reaches_the_state() {
        use crate::test_support::sine;
        // 800 Hz: 60 frames a period, so 4 800 frames are whole periods.
        let tone = sine(800.0, 48_000.0, 64 * 400);
        let mut blocks = tone.chunks(64);
        let mut f = prepared(SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(800.0))
                .q(Q::new(0.7)),
        ));
        let mut next = |f: &mut SvfFilterNode<f32>| {
            drive_block(f, &[blocks.next().expect("a block of the tone")]).remove(0)
        };
        next(&mut f);
        f.param_set()
            .cell(UnitParam::Cutoff)
            .unwrap()
            .store(f64::NAN, std::sync::atomic::Ordering::Release);
        let held = next(&mut f);
        assert!(held.iter().all(|s| s.is_finite()), "a NaN alone");
        assert!(f.param_set().set(UnitParam::Q, 3.0));
        let moved = next(&mut f);
        assert!(
            moved.iter().all(|s| s.is_finite()),
            "a NaN with another control moving in the same block"
        );
        // Held at the last good 800 Hz: a tone at the cutoff comes out at Q times itself.
        let mut level = |f: &mut SvfFilterNode<f32>| {
            for _ in 0..25 {
                next(f);
            }
            let last: Vec<f32> = (0..75).flat_map(|_| next(f)).collect();
            rms(&last)
        };
        let at_cutoff = level(&mut f);
        let want = lp_rms(800.0, 800.0, 3.0);
        assert!(
            (at_cutoff - want).abs() < 0.01 * want,
            "held at the last good cutoff: {at_cutoff} vs {want}"
        );
        assert!(f.param_set().set(UnitParam::Cutoff, 2_000.0));
        let moved_on = level(&mut f);
        let want = lp_rms(800.0, 2_000.0, 3.0);
        assert!(
            (moved_on - want).abs() < 0.01 * want,
            "and back on a finite write: {moved_on} vs {want}"
        );
    }
}
