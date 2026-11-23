//! Moog-style four-stage ladder filter with resonance and drive.
//!
//! Unlike the SVF this is a *nonlinear* filter: a `tanh` saturator sits at each of the four
//! stages' inputs and in the resonance feedback, so pushing resonance or drive grits and
//! compresses rather than blowing up. That saturation is the character, not a
//! safety measure. The implicit feedback loop is solved every sample, so the node is
//! [`Cost::Heavy`](tutti_graph::Cost::Heavy).
//!
//! One width-generic node, [`LadderFilterNode`]. The algorithm is `tutti-dsp`'s
//! ([`LadderBank`]); the node is a [`KernelNode`] over it, and this file holds the
//! kernel, the cells and how a block reads them.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, LadderBank, LadderBlend, LadderControls, LadderModel, LadderSaturation,
    LadderType, Ramp, SimdLanes,
};
use tutti_graph::Shape;
use tutti_units::{Drive, Hz, Resonance, SampleRate, Tail};

use super::filter_bus;
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// A Moog-style four-stage ladder filter with resonance and drive, of any
/// width: `N` audio inputs, `N` outputs.
///
/// The coefficients are solved once for every channel, and the channels'
/// four-stage chains run side by side.
///
/// Cutoff ([`Hz`]), [`Resonance`] and [`Drive`] are live [`Params`](crate::Params) shared
/// across clones. A held value costs nothing: the model is designed again only
/// when the cutoff or resonance moves past a small dead-band or the drive moves
/// at all ([`LadderControls::differs_from`]). The filter is designed on a 16-frame
/// grid counted from the last reset, at each segment's first frame, and the
/// coefficients **and the drive** ramp linearly to that solve over the segment, so
/// an automated control ramps rather than stepping (drive multiplies straight
/// into the saturator and would otherwise click), arrives within 16 frames and
/// renders the same however the stream is cut into blocks. A cell that moved
/// ramps from the last solve to its value across the block, so it is the one
/// control whose render depends on the block's length.
///
/// **Resonance is `0.0..=1.0`, not a [`Q`](tutti_units::Q).** It scales the
/// ladder's feedback: `0.0` is no emphasis, and toward `1.0` the filter
/// resonates hard at the cutoff and approaches self-oscillation. The feedback
/// and each stage run through a `tanh` saturator, so it grits rather than blowing up.
///
/// [`Drive`] multiplies the input into the loop's saturator, so it is a
/// distortion control rather than a level one: above unity it adds harmonics
/// and compresses. `1.0` is clean.
///
/// `F` is the float of the graph it runs in, and of the stages and the coefficients.
///
/// # Modulated params
///
/// `N` audio inputs, `N` outputs. Cutoff, Q and drive are modulatable by the
/// graph, in that port order. A per-frame value on a param port
/// ([`Io::param`](tutti_graph::Io::param)) **overrides** the corresponding cell
/// and is sampled at the first frame of each 16-frame segment, with the
/// coefficients and the drive interpolated over the segment. A control signal on the cutoff
/// or on Q stops the cutoff and Q cells ramping. Unmodulated, the filter reads its
/// cells once per block at the cost of one branch.
///
/// The param is `UnitParam::Q` for consistency with the other filters, but
/// it carries [`Resonance`] (`0.0..=1.0`), not a [`Q`](tutti_units::Q).
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, its controls are
/// a [`ParamSet`](tutti_graph::ParamSet) over cutoff, resonance (as `UnitParam::Q`)
/// and drive, and a fork of it starts from the values last set through that set.
/// Its tail is [`Tail::Unknown`]: a resonant ladder
/// rings on after its input stops, so it is never skipped on a silent block and
/// never parks. The graph prepares it at the device rate before its first block.
/// Rendering allocates nothing.
///
/// Made from a [`LadderFilterConfig`] (`LadderFilterNode::from`).
///
/// # Panics
///
/// `LadderFilterNode::from` panics past 16 channels: a filter is one bus.
pub type LadderFilterNode<F> = KernelNode<LadderKernel<F>>;

/// What a [`LadderFilterNode`] is made from: its width and its controls' start.
///
/// A config starts at a 1 kHz ladder of the given type with no resonance and unity
/// [`Drive`] ([`LadderControls::DEFAULT`]); the options set the cutoff, the resonance and the saturation model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderFilterConfig {
    width: ChannelLayout,
    controls: LadderControls,
    saturation: LadderSaturation,
}

impl LadderFilterConfig {
    /// A ladder filter of `width` channels with the tap `ladder_type`.
    ///
    /// All channels share the authored params, one linked control surface, and one
    /// coefficient solve; only the four stage words are replicated per channel.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, ladder_type: LadderType) -> Self {
        Self {
            width: width.into(),
            controls: LadderControls {
                ty: ladder_type,
                ..LadderControls::DEFAULT
            },
            saturation: LadderSaturation::PerStage,
        }
    }

    /// The cutoff at the start.
    ///
    /// The coefficients are solved at the rate [`prepare`](tutti_graph::Node::prepare)
    /// hands the node.
    #[must_use]
    pub fn freq(mut self, frequency: Hz) -> Self {
        self.controls.freq = frequency;
        self
    }

    /// The resonance at the start, held within `0.0..=1.0`: `0.0` gives no emphasis at the
    /// cutoff and values near `1.0` approach self-oscillation.
    #[must_use]
    pub fn res(mut self, resonance: Resonance) -> Self {
        self.controls.res = Resonance::new_clamped(resonance.get());
        self
    }

    /// Where the saturators sit, [`PerStage`](LadderSaturation::PerStage) unless set: the
    /// cheap [`Input`](LadderSaturation::Input) model costs an order of magnitude less per
    /// sample. Not a live control: the model is fixed when the node is made.
    #[must_use]
    pub fn saturation(mut self, saturation: LadderSaturation) -> Self {
        self.saturation = saturation;
        self
    }
}

impl<F: Float + SimdLanes> From<LadderFilterConfig> for LadderFilterNode<F> {
    fn from(config: LadderFilterConfig) -> Self {
        Self::with_kernel(
            LadderKernel::new(filter_bus(config.width), config.saturation),
            LadderSet(config.controls.ty),
            config.controls,
        )
    }
}

/// The steps the design takes to a new one: one grid period, so a design arrives at the
/// next grid point. The ramp is a ramp (a control moving to a target), not a fade.
const RAMP_STEPS: u32 = 16;

/// The ladder as a [`Kernel`]: a [`LadderBank`] in the float `F` and the ramp of the
/// design (coefficients and drive) every channel shares.
#[derive(Clone, Debug)]
pub struct LadderKernel<F: Float + SimdLanes> {
    input: ChannelTopology,
    bank: LadderBank<F>,
    /// The blend in flight; the first one after `prepare` lands at once.
    blend: Ramp<LadderBlend<F>>,
    rate: SampleRate,
    applied: bool,
}

impl<F: Float + SimdLanes> LadderKernel<F> {
    /// A filter on the bus `input`, its output where it is. Allocates the state. Its design
    /// until `prepare` is a placeholder at 48 kHz: an unprepared node renders nothing, and
    /// `prepare` designs for the rate it is given.
    fn new(input: ChannelTopology, saturation: LadderSaturation) -> Self {
        let rate = SampleRate::SR_48K;
        Self {
            input,
            bank: LadderBank::new(input.width()).saturation(saturation),
            blend: Ramp::new(Self::design(&LadderControls::DEFAULT, rate)),
            rate,
            applied: false,
        }
    }

    fn design(controls: &LadderControls, rate: SampleRate) -> LadderBlend<F> {
        LadderBlend::from(&LadderModel::new(*controls, rate))
    }
}

impl<F: Float + SimdLanes> Kernel for LadderKernel<F> {
    type Float = F;
    type Set = LadderSet;

    // A design arrives at the next grid point: the ramp is one grid period long.
    const GRID: usize = RAMP_STEPS as usize;

    fn shape(&self) -> Shape {
        Shape::through(self.input)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.blend = Ramp::new(Self::design(&LadderControls::DEFAULT, rate)).steps(RAMP_STEPS);
        self.applied = false;
    }

    fn reset(&mut self) {
        self.blend.jump_to_target();
        self.bank.reset();
    }

    fn retarget(&mut self, controls: &LadderControls) {
        self.blend.retarget(Self::design(controls, self.rate));
        if !self.applied {
            self.blend.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.bank.process(&mut self.blend, block)
    }

    /// A resonant ladder rings on after its input stops, and at a resonance of 1 it does not
    /// stop: unknown, so the graph's tail fold never reads it as none.
    fn tail(&self) -> Tail {
        Tail::Unknown
    }
}

/// The controls of a [`LadderFilterNode`]: cutoff, resonance and drive, with the
/// type they are for.
#[derive(Clone, Copy, Debug)]
pub struct LadderSet(LadderType);

// The resonance is addressed as `Q`, in `Resonance`.
control_set! {
    LadderSet(s): LadderControls = LadderControls { ty: s.0, ..LadderControls::DEFAULT };
    live: (all);
    Cutoff => Hz = freq,
    Q => Resonance = res,
    Drive => Drive = drive;

    fn moved(&self, applied: &LadderControls, target: &LadderControls) -> bool {
        target.differs_from(applied)
    }

    fn defaults(&self) -> LadderControls {
        LadderControls::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    // These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.
    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use crate::test_support::{RATE_44K, prepared_at, render_f64, tick, tick_fed};
    use tutti_dsp::Complex64;
    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    /// Gain (dB) of a 0.01-amplitude sine at `f` through the node, projected
    /// on the last second of two.
    fn gain_db(ty: LadderType, fc: f64, res: f64, f: f64) -> f64 {
        let node = LadderFilterNode::<f64>::from(
            LadderFilterConfig::new(ChannelLayout::MONO, ty)
                .freq(Hz::new(fc))
                .res(Resonance::new(res)),
        );
        let n = 44100;
        let amp = 1e-4;
        let x: Vec<f64> = (0..2 * n)
            .map(|i| amp * (2.0 * core::f64::consts::PI * f * i as f64 / 44100.0).sin())
            .collect();
        let y = render_f64(node, SampleRate::SR_44K1, &[&x], 2 * n).remove(0);
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &yi) in y.iter().enumerate().skip(n) {
            let ph = 2.0 * core::f64::consts::PI * f * i as f64 / 44100.0;
            re += yi * ph.sin();
            im += yi * ph.cos();
        }
        20.0 * (2.0 * (re * re + im * im).sqrt() / n as f64 / amp).log10()
    }

    /// Well below the cutoff the high-passes fall 12 and 24 dB per octave
    /// (`(s/(1+s))^2` and `^4`), and resonance still lifts the cutoff above
    /// the same filter without it. The probe is `1e-4` so the stages' saturators stay linear:
    /// each leaves `y^2 / 3` of its signal (3e-9 here) in the tap mix, against the 24 dB
    /// high-pass's gain of `(100 / 5000)^4 = 1.6e-7` at its lowest probe (2% of it).
    #[test]
    fn the_high_passes_have_their_documented_slopes() {
        for (ty, want) in [(LadderType::HP12, 12.0), (LadderType::HP24, 24.0)] {
            for (lo, hi) in [(100.0, 200.0), (200.0, 400.0)] {
                let slope = gain_db(ty, 5000.0, 0.0, hi) - gain_db(ty, 5000.0, 0.0, lo);
                assert!(
                    (slope - want).abs() < 1.0,
                    "{ty:?} {lo}->{hi} Hz: {slope:.2} dB/oct, want {want}"
                );
            }
            let flat = gain_db(ty, 5000.0, 0.0, 5000.0);
            let peak = gain_db(ty, 5000.0, 0.7, 5000.0);
            assert!(
                peak > flat + 3.0,
                "{ty:?} resonance: {flat:.2} -> {peak:.2}"
            );
        }
    }

    // ── Modulated params (the graph's param feed) ───────────────────────────

    /// Run a stereo ladder a frame at a time with `params` (cutoff, Q,
    /// drive: `Some` held at a value, `None` reading its base) on its param
    /// ports each frame.
    fn process_stereo_ladder(
        node: &mut dyn Node,
        l: &[f32],
        r: &[f32],
        params: [Option<f32>; 3],
    ) -> (Vec<f32>, Vec<f32>) {
        let (mut out_l, mut out_r) = (Vec::new(), Vec::new());
        let mut o = [0.0f32; 2];
        for (&a, &b) in l.iter().zip(r) {
            tick_fed(node, &[a, b], &params, &mut o);
            out_l.push(o[0]);
            out_r.push(o[1]);
        }
        (out_l, out_r)
    }

    #[test]
    fn ladder_with_channels_reports_arity_and_is_independent() {
        let mut wide = prepared_at(
            LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(6u16, LadderType::LP24)
                    .freq(Hz::new(1000.0))
                    .res(Resonance::new(0.3)),
            ),
            RATE_44K,
        );
        assert_eq!(wide.shape().audio_in().count(), 6);
        assert_eq!(wide.shape().audio_out().count(), 6);

        // Drive only channel 4; the rest must stay silent.
        let sig = generate_sine(300.0, 44100.0, 2048);
        let len = sig.len();
        let mut out = vec![vec![0.0f32; len]; 6];
        let mut outbuf = [0.0f32; 6];
        for i in 0..len {
            // Rebuilt per sample, not cleared in place: only channel 4 is
            // driven, so every other channel must re-enter each frame at zero.
            let mut inbuf = [0.0f32; 6];
            inbuf[4] = sig[i];
            tick(&mut wide, &inbuf, &mut outbuf);
            for c in 0..6 {
                out[c][i] = outbuf[c];
            }
        }
        let e4: f32 = out[4].iter().map(|s| s * s).sum();
        assert!(e4 > 1.0, "ch4 should carry signal; energy {e4}");
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
    }

    #[test]
    fn stereo_ladder_fed_cutoff_modulates_response() {
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let run = |cutoff: f32| -> f64 {
            let mut f = prepared_at(
                LadderFilterNode::<f32>::from(
                    LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                        .freq(Hz::new(200.0))
                        .res(Resonance::new(0.3)),
                ),
                RATE_44K,
            );
            let (out_l, _) =
                process_stereo_ladder(&mut f, &noise, &noise, [Some(cutoff), None, None]);
            rms(&out_l[256..])
        };
        let low = run(200.0);
        let high = run(8000.0);
        assert!(
            high > low * 1.5,
            "higher fed cutoff should pass more: low={low}, high={high}"
        );
    }

    #[test]
    fn stereo_ladder_unmodulated_matches_modulated_held_constant() {
        // A node whose fed cutoff is held at the atomic value must produce the
        // same output as a plain node.
        let signal = generate_sine(440.0, 44100.0, 1024);

        let mut plain = prepared_at(
            LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                    .freq(Hz::new(1000.0))
                    .res(Resonance::new(0.3)),
            ),
            RATE_44K,
        );
        let (plain_l, _) = process_stereo_ladder(&mut plain, &signal, &signal, [None; 3]);

        let mut modn = prepared_at(
            LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                    .freq(Hz::new(1000.0))
                    .res(Resonance::new(0.3)),
            ),
            RATE_44K,
        );
        let (mod_l, _) =
            process_stereo_ladder(&mut modn, &signal, &signal, [Some(1000.0), None, None]);

        for i in 0..signal.len() {
            assert!(
                (plain_l[i] - mod_l[i]).abs() < 1e-5,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain_l[i],
                mod_l[i]
            );
        }
    }

    // ── Per-block reads ──────────────────────────────────────────────────────

    /// A cutoff written between blocks ramps on the stream clock: the render is the
    /// same at every block length, under way one 64-frame block in, and arrived
    /// [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP) (240 frames at 48 kHz) after the block it
    /// was first seen in.
    #[test]
    fn a_cutoff_written_between_blocks_ramps_on_the_stream() {
        const FS: f64 = 48_000.0;
        const HZ: f64 = 1_500.0;
        const RES: f64 = 0.4;
        let make = || {
            LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                    .freq(Hz::new(1_000.0))
                    .res(Resonance::new(RES)),
            )
        };
        let change = |n: &LadderFilterNode<f32>| {
            assert!(n.param_set().set(UnitParam::Cutoff, 1_400.0));
        };
        // 1e-3 keeps the saturator linear (`tanh x = x - x^3/3`: 3e-7 of the signal).
        let (low, blocks) = block_levels(make, change, HZ, 1e-3);

        // The spec: `1 / ((1 + s/wc)^4 + k)`, `k = 4 res`, under the bilinear map prewarped
        // at the cutoff, the same figure the bank's held-response test pins.
        let gain = |g: f64| {
            let one = Complex64::new(1.0, 0.0);
            let zi = Complex64::from_polar(1.0, -2.0 * core::f64::consts::PI * HZ / FS);
            let s = (one - zi) / (one + zi) / g;
            let p = one + s;
            1.0 / (p * p * p * p + 4.0 * RES).norm()
        };
        let g_of = |fc: f64| libm::tan(core::f64::consts::PI * fc / FS);
        // The cell ramps 1000 to 1400 Hz over 240 frames from the write at stream frame 256
        // (the grid point at its first block): step `k = frame - 255` at a grid point's
        // frame. A design is taken at each grid point (every 16 frames) and the
        // coefficients ramp to it over the 16 frames after: frame `t` is `(t - p + 1) / 16`
        // of the way from the design at the grid point `p` before it to the one at `p`.
        let cell = |p: i64| {
            let k = (p - 255).clamp(0, 240) as f64;
            1_000.0 + 400.0 * k / 240.0
        };
        let expect = |block: usize| {
            let mut sum = 0.0;
            for i in 0..64 {
                let t = 256 + 64 * block as i64 + i;
                let p = t - t % 16;
                let w = (t - p + 1) as f64 / 16.0;
                let g = g_of(cell(p - 16)) * (1.0 - w) + g_of(cell(p)) * w;
                sum += gain(g).powi(2);
            }
            1e-3 * core::f64::consts::FRAC_1_SQRT_2 * (sum / 64.0).sqrt()
        };
        // The tone is steady-state at each block's design, so the ramp's lag in the stages
        // costs a few percent; a cutoff that never moved or jumped is off by tens.
        let near = |got: f64, want: f64, what: &str| {
            assert!(
                (got - want).abs() < 0.10 * want,
                "{what}: {got} against {want}"
            );
        };
        near(
            low,
            1e-3 * core::f64::consts::FRAC_1_SQRT_2 * gain(g_of(1_000.0)),
            "before",
        );
        for (j, &level) in blocks.iter().enumerate() {
            near(level, expect(j), &format!("block {j} after the write"));
        }
        // A cutoff that jumped would put block 0 at the final gain, three times the
        // held one; the ramp's first block is well under it.
        let arrived = 1e-3 * core::f64::consts::FRAC_1_SQRT_2 * gain(g_of(1_400.0));
        assert!(blocks[0] < 0.8 * arrived, "block 0 is not yet arrived");
    }

    /// Drive multiplies straight into the saturator, so a step would click: a drive
    /// written between blocks ramps on the stream clock like a cutoff, and arrives. The
    /// level of a high-pass tone below the saturator's knee rises on every block of the ramp
    /// (a drive that jumped would start at its final level, and one that never moved would
    /// not rise), starts well under it, and settles on it.
    #[test]
    fn a_drive_written_between_blocks_ramps_on_the_stream() {
        // A high-pass tap, so the saturator's input reaches the output unfiltered.
        let make = || {
            LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::HP24)
                    .freq(Hz::new(500.0))
                    .res(Resonance::new(0.4)),
            )
        };
        let change = |n: &LadderFilterNode<f32>| {
            assert!(n.param_set().set(UnitParam::Drive, 9.0));
        };
        let (low, blocks) = block_levels(make, change, 6_000.0, 0.1);
        let arrived = blocks[7];
        assert!(arrived > 4.0 * low, "the drive arrived: {low} to {arrived}");
        assert!(
            blocks[0] > low && blocks[0] < 0.5 * arrived,
            "block 0 is under way: {low} < {} < {arrived}",
            blocks[0]
        );
        // Four 64-frame blocks of a 240-frame ramp: each one louder than the last, then
        // at rest (the ramp is done by block 4).
        assert!(blocks[..5].windows(2).all(|w| w[1] > w[0]), "{blocks:?}");
        assert!(
            blocks[5..]
                .iter()
                .all(|&l| (l - arrived).abs() < 0.01 * arrived)
        );
    }

    /// A stereo node over a tone of `hz` (whole periods in 64 frames) at `amplitude`:
    /// the RMS of the last 64 frames of 256 before a write (`low`), and of each 64-frame
    /// block of the 512 frames after it. The render is also the same at every block length.
    fn block_levels(
        make: impl Fn() -> LadderFilterNode<f32>,
        change: impl Fn(&LadderFilterNode<f32>),
        hz: f64,
        amplitude: f32,
    ) -> (f64, Vec<f64>) {
        use crate::test_support::{assert_cell_ramps, drive_block, prepared, render_change, sine};
        let x: Vec<f32> = sine(hz, 48_000.0, 256 + 512)
            .into_iter()
            .map(|s| amplitude * s)
            .collect();
        let (before, after) = ([&x[..256], &x[..256]], [&x[256..], &x[256..]]);
        assert_cell_ramps(&make, &change, &before, &after, "ladder");
        let mut start = prepared(make());
        let low = rms(&drive_block(&mut start, &before)[0][192..]);
        let (_, out) = render_change(&make, &change, &before, &after, 64);
        (low, out[0].chunks(64).map(rms).collect())
    }

    /// Six channels share one coefficient solve and keep six independent
    /// stage lanes: each channel is the mono ladder on its own input.
    #[test]
    fn six_channels_are_six_mono_ladders() {
        use crate::test_support::{drive_block, noise, prepared};
        let inputs: Vec<Vec<f32>> = (0..6).map(|c| noise(c + 20, 64)).collect();
        let refs: Vec<&[f32]> = inputs.iter().map(|v| &v[..]).collect();
        let mut wide = prepared(LadderFilterNode::<f32>::from(
            LadderFilterConfig::new(6u16, LadderType::HP12)
                .freq(Hz::new(800.0))
                .res(Resonance::new(0.7)),
        ));
        let out = drive_block(&mut wide, &refs);
        for (c, input) in inputs.iter().enumerate() {
            let mut mono = prepared(LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::MONO, LadderType::HP12)
                    .freq(Hz::new(800.0))
                    .res(Resonance::new(0.7)),
            ));
            assert_eq!(
                drive_block(&mut mono, &[&input[..]])[0],
                out[c],
                "channel {c}"
            );
        }
    }
}
