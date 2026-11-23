//! [`DiodeClipperNode`]: the RC-loaded antiparallel diode pair, oversampled.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock, Mono};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, ClipperDrive, DiodeClipper, DiodeClipperControls, Lerp,
    OversamplingFactor,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, Hz, SampleRate, Samples, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};
use crate::oversampled::Oversampled;

/// What a [`DiodeClipperNode`] is made from: its width, its controls and its oversampling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiodeClipperConfig {
    width: ChannelLayout,
    controls: DiodeClipperControls,
    factor: OversamplingFactor,
}

impl DiodeClipperConfig {
    /// A clipper of `width` channels at [`DiodeClipperControls::DEFAULT`] (5 kHz, +12 dB),
    /// oversampled 4 times: 192 kHz at 48 kHz, the rate the solver's residual is
    /// tested at (the bound holds at every factor).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            controls: DiodeClipperControls::DEFAULT,
            factor: OversamplingFactor::X4,
        }
    }

    /// The RC corner it starts at, held as [`DiodeClipperControls::cutoff`] says.
    #[must_use]
    pub fn cutoff(mut self, cutoff: Hz) -> Self {
        self.controls.cutoff = cutoff;
        self
    }

    /// The drive it starts at.
    #[must_use]
    pub fn drive(mut self, drive: ClipperDrive) -> Self {
        self.controls.drive = drive;
        self
    }

    /// The oversampling the circuit runs at; the default is 4 times.
    #[must_use]
    pub fn oversampling(mut self, factor: OversamplingFactor) -> Self {
        self.factor = factor;
        self
    }
}

/// A diode clipper of any width: every channel an RC low-pass with an antiparallel diode pair
/// across its capacitor ([`DiodeClipper`]), run at [`OversamplingFactor`] times the rate.
///
/// # Controls
///
/// The RC corner is a live [`Hz`] cell (`UnitParam::Cutoff`) and the drive a live [`Db`] cell
/// (`UnitParam::GainDb`), sampled at the stream grid (16 frames) and ramping over
/// [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP). A control signal has no caller to refuse a value to:
/// a drive out of [`ClipperDrive`]'s range reads as its nearest end
/// ([`ClipperDrive::held`]), the corner is held as [`DiodeClipperControls::cutoff`] says, and
/// a value that is not finite reads as the last one that was.
///
/// # Latency and tail
///
/// It delays the audio by the oversampler's cascade, which it declares. The tail is that
/// latency plus the frames the circuit rings after the input stops
/// ([`DiodeClipperControls::tail`]), narrowed per block to the cutoff in force.
///
/// Heavy: two Newton steps and a Wright omega per sample at the oversampled rate. A
/// [`KernelNode`], so a non-finite sample is heard in at most one block. Made from a
/// [`DiodeClipperConfig`] (`DiodeClipperNode::from`).
///
/// # Panics
///
/// `DiodeClipperNode::from` panics past 16 channels: a clipper is one bus.
///
/// `F` is the float of the graph it runs in, and of the solve.
pub type DiodeClipperNode<F> = KernelNode<DiodeClipperKernel<F>>;

impl<F: Float + Lerp> From<DiodeClipperConfig> for DiodeClipperNode<F> {
    fn from(config: DiodeClipperConfig) -> Self {
        let input = unassigned_bus(config.width, "a diode clipper");
        Self::with_kernel(
            DiodeClipperKernel::<F>::new(&config, input),
            DiodeClipperSet,
            config.controls,
        )
    }
}

/// The cells of a [`DiodeClipperNode`]: the corner and the drive.
#[derive(Clone, Copy, Debug)]
pub struct DiodeClipperSet;

control_set! {
    DiodeClipperSet(_s): DiodeClipperControls = DiodeClipperControls::DEFAULT;
    live: (all);
    Cutoff => Hz = cutoff,
    GainDb => Db = drive via ClipperDrive;

    fn defaults(&self) -> DiodeClipperControls {
        DiodeClipperControls::DEFAULT
    }
}

/// The [`Kernel`] of a [`DiodeClipperNode`].
#[derive(Clone, Debug)]
pub struct DiodeClipperKernel<F: Float> {
    input: BusTopology,
    factor: OversamplingFactor,
    /// The rate the circuit runs at: the oversampled one, or the stream's where no half-band
    /// can be designed.
    rate: SampleRate,
    clipper: DiodeClipper<F>,
    applied: bool,
    oversampled: Option<Oversampled<F>>,
}

impl<F: Float + Lerp> DiodeClipperKernel<F> {
    fn new(config: &DiodeClipperConfig, input: BusTopology) -> Self {
        Self {
            input,
            factor: config.factor,
            rate: SampleRate::SR_48K,
            clipper: DiodeClipper::<F>::new(config.width, SampleRate::SR_48K),
            applied: false,
            oversampled: None,
        }
    }

    /// The frames of the stream the controls ring: the circuit's, at the oversampled rate,
    /// over the factor, and the cascade.
    fn ring(&self, controls: &DiodeClipperControls) -> Samples {
        let (factor, cascade) = self.oversampled.as_ref().map_or((1, Samples::ZERO), |o| {
            (o.plan.factor().get(), o.plan.latency())
        });
        let circuit = controls.tail(self.rate).get().div_ceil(factor);
        Samples::new(circuit + cascade.get() + 1)
    }
}

impl<F: Float + Lerp> Kernel for DiodeClipperKernel<F> {
    type Float = F;
    type Set = DiodeClipperSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Heavy)
    }

    fn prepare(&mut self, rate: SampleRate, max_block: MaxBlock) {
        let width = usize::from(self.input.width().count());
        self.oversampled = Oversampled::new(self.factor, rate, max_block, width);
        self.rate = self
            .oversampled
            .as_ref()
            .map_or(rate, |o| o.plan.high_rate());
        self.clipper = DiodeClipper::<F>::new(self.input.width(), self.rate);
        self.applied = false;
    }

    fn reset(&mut self) {
        self.clipper.reset();
        self.applied = false;
        for state in self.oversampled.iter_mut().flat_map(|o| &mut o.channels) {
            state.reset();
        }
    }

    fn retarget(&mut self, controls: &DiodeClipperControls) {
        self.clipper.retarget(controls);
        if !self.applied {
            self.clipper.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let rendered = match &mut self.oversampled {
            Some(oversampled) => oversampled.render(None::<Mono<'_, F>>, block, |_, wide| {
                self.clipper.process(wide)
            }),
            None => self.clipper.process(block),
        };
        // A design after a render is a move, which ramps.
        self.applied = true;
        rendered
    }

    fn latency(&self) -> Samples {
        self.oversampled
            .as_ref()
            .map_or(Samples::ZERO, |o| o.plan.latency())
    }

    /// The ring-out at the lowest cutoff: the longest over every control.
    fn tail(&self) -> Tail {
        Tail::Finite(self.ring(&DiodeClipperControls {
            cutoff: DiodeClipperControls::MIN_CUTOFF,
            ..DiodeClipperControls::DEFAULT
        }))
    }

    fn tail_of(&mut self, controls: &DiodeClipperControls) -> Tail {
        Tail::Finite(self.ring(controls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{amplitude_at, drive_block, prepared, render_chunked, sine};

    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    const RATE: f64 = 48_000.0;

    fn node(cutoff: f64, drive: f64) -> DiodeClipperNode<f32> {
        prepared(DiodeClipperNode::<f32>::from(
            DiodeClipperConfig::new(1u16)
                .cutoff(Hz::new(cutoff))
                .drive(ClipperDrive::of(Db::new(drive))),
        ))
    }

    /// The impulse of a linear-range input, at the highest cutoff, peaks at the oversampler's
    /// declared latency (the circuit's own delay is under a frame there) and has rung out
    /// by the declared tail.
    #[test]
    fn the_oversampler_delays_the_audio_by_the_declared_latency_and_the_tail_covers_the_ring() {
        for factor in [
            OversamplingFactor::X2,
            OversamplingFactor::X4,
            OversamplingFactor::X8,
        ] {
            let mut n = prepared(DiodeClipperNode::<f32>::from(
                DiodeClipperConfig::new(1u16)
                    .cutoff(Hz::new(1.0e6))
                    .drive(ClipperDrive::of(Db::new(-40.0)))
                    .oversampling(factor),
            ));
            let latency = n.shape().latency.samples().get();
            let Tail::Finite(tail) = n.shape().tail else {
                panic!("{factor:?}: no finite tail");
            };
            assert!(latency > 0, "{factor:?}");
            let mut x = vec![0.0f32; 40_000];
            x[0] = 0.5;
            let y = render_chunked(&mut n, &[x], 100).remove(0);
            let peak = (0..y.len())
                .max_by(|&a, &b| y[a].abs().total_cmp(&y[b].abs()))
                .unwrap();
            assert_eq!(peak, latency, "{factor:?}");
            assert!(
                y[latency + tail.get() + 1..]
                    .iter()
                    .all(|&s| s.abs() < 1e-6),
                "{factor:?}: rings past the declared tail"
            );
        }
    }

    /// The corner is a live cell: moved from 4 kHz to 500 Hz, the gain at 2 kHz follows the RC
    /// closed form of the new one once the 5 ms ramp is done.
    #[test]
    fn the_cutoff_is_settable_by_unit_param_and_lands_on_the_new_corner() {
        let mut n = node(4_000.0, 0.0);
        assert!(n.param_set().set(UnitParam::Cutoff, 500.0));
        let f = 2_000.0;
        let x: Vec<f32> = sine(f, RATE, 9_600).iter().map(|v| v * 1e-4).collect();
        let y = render_chunked(&mut n, &[x], 256).remove(0);
        let gain = amplitude_at(&y[4_800..], f, RATE) / 1e-4;
        let analog = 1.0 / (1.0 + (f / 500.0f64).powi(2)).sqrt();
        assert!((gain - analog).abs() <= 1e-3, "{gain} vs {analog}");
        assert!(!n.param_set().set(UnitParam::Drive, 2.0));
    }

    /// A drive step of +20 dB raises a clipped output by less than the 10 times it raises the
    /// input: the diodes clip.
    #[test]
    fn the_drive_is_settable_by_unit_param_and_the_pair_clips() {
        let mut n = node(5_000.0, 0.0);
        let x: Vec<f32> = sine(1_000.0, RATE, 4_800).iter().map(|v| v * 0.5).collect();
        let before = render_chunked(&mut n, std::slice::from_ref(&x), 256).remove(0);
        assert!(n.param_set().set(UnitParam::GainDb, 20.0));
        let _ = drive_block(&mut n, &[&x[..64]]);
        let after = render_chunked(&mut n, &[x], 256).remove(0);
        let (a, b) = (
            amplitude_at(&before[2_400..], 1_000.0, RATE),
            amplitude_at(&after[2_400..], 1_000.0, RATE),
        );
        assert!(b > a && b < 2.0 * a, "{a} -> {b}");
    }
}
