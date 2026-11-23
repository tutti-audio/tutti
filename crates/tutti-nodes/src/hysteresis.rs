//! [`HysteresisNode`]: Jiles-Atherton magnetic hysteresis, oversampled.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock, Mono};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, Hysteresis, HysteresisControls, HysteresisDrive, Lerp,
    OversamplingFactor,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_units::{Db, Normalized, SampleRate, Samples, Tail, UnitAddr, UnitParam};

use crate::kernel::{ControlSignals, Kernel, KernelNode};
use crate::oversampled::Oversampled;
use crate::params::{ControlSet, HasControl};
use tutti_graph::DeclaredUnit;

/// What a [`HysteresisNode`] is made from: its width, its material and drive, and its
/// oversampling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HysteresisConfig {
    width: ChannelLayout,
    controls: HysteresisControls,
    saturation: Normalized,
    coercivity: Normalized,
    factor: OversamplingFactor,
}

impl HysteresisConfig {
    /// A stage of `width` channels at [`HysteresisControls::DEFAULT`] (+12 dB), of the
    /// material `Hysteresis` documents (saturation 0.2, coercivity position 0.5), oversampled
    /// 4 times.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            controls: HysteresisControls::DEFAULT,
            saturation: Normalized::of(0.2),
            coercivity: Normalized::of(0.5),
            factor: OversamplingFactor::X4,
        }
    }

    /// The drive it starts at.
    #[must_use]
    pub fn drive(mut self, drive: HysteresisDrive) -> Self {
        self.controls.drive = drive;
        self
    }

    /// The share of the magnetization that is reversible, `0..=1`: 0 is the widest loop the
    /// coercivity allows and 1 a memoryless saturating curve.
    #[must_use]
    pub fn saturation(mut self, saturation: Normalized) -> Self {
        self.saturation = saturation;
        self
    }

    /// The loop's width, `0..=1`: the pinning of the material, on a scale from a sixteenth of
    /// the set's at 0 through the set's at 0.5 to sixteen times it at 1.
    #[must_use]
    pub fn coercivity(mut self, coercivity: Normalized) -> Self {
        self.coercivity = coercivity;
        self
    }

    /// The oversampling the material runs at; the default is 4 times.
    #[must_use]
    pub fn oversampling(mut self, factor: OversamplingFactor) -> Self {
        self.factor = factor;
        self
    }
}

/// A hysteresis stage of any width: every channel a piece of magnetic material whose
/// magnetization follows the signal as its field ([`Hysteresis`]), run at
/// [`OversamplingFactor`] times the rate. The output is the magnetization in units of the
/// saturation.
///
/// # Controls
///
/// The drive is a live [`Db`] cell (`UnitParam::GainDb`), sampled at the stream grid (16
/// frames) and ramping over [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP). A control signal has no caller
/// to refuse a value to: a drive out of [`HysteresisDrive`]'s range reads as its nearest end
/// ([`HysteresisDrive::held`]).
///
/// # Latency and tail
///
/// It delays the audio by the oversampler's cascade, which it declares. The tail is unbounded:
/// the material keeps its remanence when the signal stops, a constant that never decays, which
/// a DC blocker after the node removes.
///
/// Heavy: up to four Newton steps per substep and up to 128 substeps per sample at the
/// oversampled rate (one for a field that moves under 0.25 `a` a sample). A [`KernelNode`], so
/// a non-finite sample is heard in at most one block. Made from a [`HysteresisConfig`]
/// (`HysteresisNode::from`).
///
/// # Panics
///
/// `HysteresisNode::from` panics past 16 channels: a stage is one bus.
///
/// `F` is the float of the graph it runs in, and of the solve.
pub type HysteresisNode<F> = KernelNode<HysteresisKernel<F>>;

impl<F: Float + Lerp> From<HysteresisConfig> for HysteresisNode<F> {
    fn from(config: HysteresisConfig) -> Self {
        let input = BusTopology::unassigned(config.width)
            .expect("a hysteresis stage is at most 16 channels wide");
        Self::with_kernel(
            HysteresisKernel::<F>::new(&config, input),
            HysteresisSet,
            config.controls,
        )
    }
}

/// The cell of a [`HysteresisNode`]: the drive.
#[derive(Clone, Copy, Debug)]
pub struct HysteresisSet;

/// The one control is the drive, a range-checked [`HysteresisDrive`], which `control_set!` does
/// not take (its rows are units), so the set is written out.
impl ControlSet for HysteresisSet {
    type Controls = HysteresisControls;

    fn addrs(&self) -> &'static [UnitAddr] {
        const ALL: &[UnitAddr] = &[UnitAddr::first(UnitParam::GainDb)];
        ALL
    }

    fn units(&self) -> &'static [DeclaredUnit] {
        const ALL: &[DeclaredUnit] = &[DeclaredUnit::of::<Db>()];
        ALL
    }

    fn defaults(&self) -> HysteresisControls {
        HysteresisControls::DEFAULT
    }

    fn flatten(&self, c: &HysteresisControls, out: &mut [f64]) {
        let all = [c.drive.get().get()];
        out.copy_from_slice(&all[..out.len()]);
    }

    fn assemble(&self, v: &[f64]) -> HysteresisControls {
        let mut c = HysteresisControls::DEFAULT;
        if let Some(&x) = v.first() {
            // The one place a live drive is changed: a cell is written per frame, with no
            // caller to refuse it to.
            c.drive = HysteresisDrive::held(Db::new(x));
        }
        c
    }
}

impl HasControl<{ UnitParam::GainDb as u16 }> for HysteresisSet {
    type Unit = Db;
}

/// The [`Kernel`] of a [`HysteresisNode`].
#[derive(Clone, Debug)]
pub struct HysteresisKernel<F: Float> {
    input: BusTopology,
    factor: OversamplingFactor,
    saturation: Normalized,
    coercivity: Normalized,
    stage: Hysteresis<F>,
    applied: bool,
    oversampled: Option<Oversampled<F>>,
}

impl<F: Float + Lerp> HysteresisKernel<F> {
    fn new(config: &HysteresisConfig, input: BusTopology) -> Self {
        Self {
            input,
            factor: config.factor,
            saturation: config.saturation,
            coercivity: config.coercivity,
            stage: Hysteresis::<F>::new(
                config.width,
                SampleRate::SR_48K,
                config.saturation,
                config.coercivity,
            ),
            applied: false,
            oversampled: None,
        }
    }
}

impl<F: Float + Lerp> Kernel for HysteresisKernel<F> {
    type Float = F;
    type Set = HysteresisSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        let output = Bus::banded(self.input, BusRole::Main, 1).expect("one band is a bus");
        Shape::buses(
            Buses::main(self.input),
            Buses::NONE.with(output).expect("one bus"),
        )
        .cost(Cost::Heavy)
    }

    fn prepare(&mut self, rate: SampleRate, max_block: MaxBlock) {
        let width = usize::from(self.input.width().count());
        self.oversampled = Oversampled::new(self.factor, rate, max_block, width);
        let rate = self
            .oversampled
            .as_ref()
            .map_or(rate, |o| o.plan.high_rate());
        self.stage =
            Hysteresis::<F>::new(self.input.width(), rate, self.saturation, self.coercivity);
        self.applied = false;
    }

    fn reset(&mut self) {
        self.stage.reset();
        self.applied = false;
        for state in self.oversampled.iter_mut().flat_map(|o| &mut o.channels) {
            state.reset();
        }
    }

    fn retarget(&mut self, controls: &HysteresisControls) {
        self.stage.retarget(controls);
        if !self.applied {
            self.stage.jump_to_target();
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
                self.stage.process(wide)
            }),
            None => self.stage.process(block),
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

    /// The remanence never decays.
    fn tail(&self) -> Tail {
        Tail::Unbounded
    }

    fn tail_of(&mut self, _: &HysteresisControls) -> Tail {
        Tail::Unbounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{amplitude_at, drive_block, prepared, render_chunked, sine};
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    const RATE: f64 = 48_000.0;

    fn node(drive: f64) -> HysteresisNode<f32> {
        prepared(HysteresisNode::<f32>::from(
            HysteresisConfig::new(1u16).drive(HysteresisDrive::of(Db::new(drive))),
        ))
    }

    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(HysteresisNode::<f32>::from(HysteresisConfig::new(
            ChannelLayout::STEREO,
        )));
    }

    #[test]
    fn the_shape_is_heavy_unbounded_and_declares_the_drive() {
        let n = prepared(HysteresisNode::<f32>::from(HysteresisConfig::new(3u16)));
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Heavy);
        assert_eq!(shape.tail, Tail::Unbounded);
        assert_eq!(shape.params.as_slice(), &[UnitParam::GainDb][..]);
        assert_eq!(n.param_base(0), Some(12.0));
        let empty = HysteresisNode::<f32>::from(HysteresisConfig::new(0u16));
        assert_eq!(empty.shape().audio_in().count(), 0);
    }

    /// The oversampler's cascade is linear phase, so a small pulse (the material is nearly
    /// reversible at -24 dB) peaks at the declared latency, at every factor.
    #[test]
    fn the_oversampler_delays_the_audio_by_the_declared_latency() {
        for factor in [
            OversamplingFactor::X2,
            OversamplingFactor::X4,
            OversamplingFactor::X8,
        ] {
            let mut n = prepared(HysteresisNode::<f32>::from(
                HysteresisConfig::new(1u16)
                    .drive(HysteresisDrive::MIN)
                    .saturation(Normalized::of(1.0))
                    .oversampling(factor),
            ));
            let latency = n.shape().latency.samples().get();
            assert!(latency > 0, "{factor:?}");
            let mut x = vec![0.0f32; 4_000];
            x[0] = 0.5;
            let y = render_chunked(&mut n, &[x], 100).remove(0);
            let peak = (0..y.len())
                .max_by(|&a, &b| y[a].abs().total_cmp(&y[b].abs()))
                .unwrap();
            assert_eq!(peak, latency, "{factor:?}");
        }
    }

    /// A drive step of +18 dB (a factor of 7.94 in the field) raises the output by less than
    /// that: the slope of the material falls toward saturation, `m < 1`.
    #[test]
    fn the_drive_is_settable_by_unit_param_and_the_material_saturates() {
        let mut n = node(0.0);
        let x: Vec<f32> = sine(1_000.0, RATE, 4_800).iter().map(|v| v * 0.5).collect();
        let before = render_chunked(&mut n, std::slice::from_ref(&x), 256).remove(0);
        assert!(n.param_set().set(UnitParam::GainDb, 18.0));
        let _ = drive_block(&mut n, &[&x[..64]]);
        let after = render_chunked(&mut n, &[x], 256).remove(0);
        let (a, b) = (
            amplitude_at(&before[2_400..], 1_000.0, RATE),
            amplitude_at(&after[2_400..], 1_000.0, RATE),
        );
        assert!(b > a && b < 7.94 * a, "{a} -> {b}");
        assert!(
            after.iter().all(|v| v.abs() < 1.1),
            "the output stays within the saturation"
        );
        assert!(!n.param_set().set(UnitParam::Cutoff, 2.0));
    }
}
