//! The dynamic EQ band [`DynamicEqNode`]: an SVF bell whose gain follows the level of a
//! band-passed key, and its de-esser configuration.

use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock};
use tutti_buffers::{Float, Planes};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, DynamicEqBank, DynamicEqControls, DynamicMode, KeySource, Lerp,
    SimdLanes,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`DynamicEqNode`] is made from: its width, the band's centre and Q, and optionally
/// the curve, the times, the range, the mode and the key.
///
/// The controls are the compressor's where the meaning is the same
/// ([`threshold`](Self::threshold), [`knee`](Self::knee), [`ratio`](Self::ratio),
/// [`attack`](Self::attack), [`release`](Self::release), [`key`](Self::key)); the
/// [`range`](Self::range) and the [`mode`](Self::mode) are the band's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicEqConfig {
    width: ChannelLayout,
    key: KeySource,
    mode: DynamicMode,
    controls: DynamicEqControls,
}

impl DynamicEqConfig {
    /// A downward band of `width` channels at `center` and `q`, listening to its own signal,
    /// that does nothing until a threshold and a ratio are given: threshold 0 dB, a hard knee,
    /// ratio 1, instant times and a range of 12 dB.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, center: Hz, q: Q) -> Self {
        Self {
            width: width.into(),
            key: KeySource::Own,
            mode: DynamicMode::Downward,
            controls: DynamicEqControls {
                center,
                q,
                ..DynamicEqControls::DEFAULT
            },
        }
    }

    /// A de-esser of `width` channels on the sibilance at `center`: a **downward** band
    /// whose detector hears the key through the band-pass at the same centre.
    ///
    /// It is a bell rather than a high shelf because the reduction acts where the detector
    /// listens: a shelf would also dull the air and the consonants above the band that the
    /// detector never judged sibilant, and a bell at the band's Q leaves them untouched.
    /// It starts at a Q of 2 (a band as wide as half its centre, which holds the spread of
    /// "s" and "sh"), a threshold of -24 dB, a ratio of 4, a 6 dB knee, a 0.5 ms attack, a 40 ms
    /// release and a range of 10 dB; each is [`threshold`](Self::threshold),
    /// [`ratio`](Self::ratio) and so on, as for any band. A voice's sibilance is usually
    /// between 4 and 9 kHz.
    #[must_use]
    pub fn de_esser(width: impl Into<ChannelLayout>, center: Hz) -> Self {
        Self::new(width, center, Q::new(2.0))
            .threshold(Db::new(-24.0))
            .ratio(CompressionRatio::new(4.0))
            .knee(Db::new(6.0))
            .attack(Seconds::new(0.0005))
            .release(Seconds::new(0.04))
            .range(Db::new(10.0))
    }

    /// The level, in dB, at which the band starts to move.
    #[must_use]
    pub fn threshold(self, threshold: Db) -> Self {
        self.with(|c| c.threshold = threshold)
    }

    /// The width of the soft knee in dB, centred on the threshold; 0 is a hard knee.
    #[must_use]
    pub fn knee(self, knee: Db) -> Self {
        self.with(|c| c.knee = knee)
    }

    /// dB in per dB out past the threshold, as the compressor's ratio; at least 1.
    #[must_use]
    pub fn ratio(self, ratio: CompressionRatio) -> Self {
        self.with(|c| c.ratio = ratio)
    }

    /// How fast the gain change grows: the time constant of a one-pole smoother.
    #[must_use]
    pub fn attack(self, attack: Seconds) -> Self {
        self.with(|c| c.attack = attack)
    }

    /// How fast the gain change falls back toward flat.
    #[must_use]
    pub fn release(self, release: Seconds) -> Self {
        self.with(|c| c.release = release)
    }

    /// The most the band's gain moves from flat, in dB and not below 0.
    #[must_use]
    pub fn range(self, range: Db) -> Self {
        self.with(|c| c.range = range)
    }

    /// Whether the band cuts above its threshold or boosts below it.
    #[must_use]
    pub fn mode(self, mode: DynamicMode) -> Self {
        Self { mode, ..self }
    }

    /// Where the detector listens: the signal itself, or a side-chain bus as wide as the
    /// signal, which **must be wired** (an unwired key is silence, which leaves a downward
    /// band flat and an upward one at its full boost).
    #[must_use]
    pub fn key(self, key: KeySource) -> Self {
        Self { key, ..self }
    }

    fn with(mut self, f: impl FnOnce(&mut DynamicEqControls)) -> Self {
        f(&mut self.controls);
        self
    }
}

/// One EQ band of any width, an SVF bell whose gain follows the level of the signal in the
/// band, linked across the channels: `channels = N` means N audio inputs, then (for a
/// side-chain key) N key inputs, and N outputs.
///
/// A [`KernelNode`] over [`DynamicEqKernel`], the detector, the curve and the bell being
/// [`DynamicEqBank`]'s (see it for the detector and the gain's ramp); this file holds the
/// cells and how the graph reaches them. Made from a [`DynamicEqConfig`]
/// (`DynamicEqNode::from`).
///
/// # The key
///
/// The detector listens to a [`KeySource`], chosen when the node is built: the signal itself
/// by default, or a second bus after the audio inputs (see [`CompressorNode`](crate::CompressorNode)
/// for the wiring). Either way it hears the key through a band-pass at the band's centre and
/// Q, and the signal the bell is applied to is filtered only by the bell.
///
/// # Controls
///
/// Threshold, knee, ratio, attack, release and range ([`UnitParam::Range`](tutti_units::UnitParam::Range),
/// [`Db`]), then the band's centre ([`UnitParam::Cutoff`](tutti_units::UnitParam::Cutoff),
/// [`Hz`]) and Q ([`UnitParam::Q`](tutti_units::UnitParam::Q), [`Q`]), are live
/// [`Params`](crate::Params) shared across clones. They are on the stream grid (16 frames from
/// the last reset, [`Kernel::GRID`]), so the render does not depend on how the stream is cut
/// into blocks; the curve and the times land whole and the bell ramps to a new centre or Q.
///
/// The tail is unknown: a bell rings for as long as its Q says.
///
/// `F` is the float of the graph it runs in, and of the detector and the bell.
pub type DynamicEqNode<F> = KernelNode<DynamicEqKernel<F>>;

impl<F: Float + SimdLanes + Lerp> From<DynamicEqConfig> for DynamicEqNode<F> {
    /// # Panics
    ///
    /// Past 16 channels: a band is one bus.
    fn from(config: DynamicEqConfig) -> Self {
        let input =
            BusTopology::default_for(config.width).expect("a band is at most 16 channels wide");
        Self::with_kernel(
            DynamicEqKernel::new(input, config.key, config.mode),
            DynamicEqSet,
            config.controls,
        )
    }
}

/// The band's controls: threshold, knee, ratio, attack, release, range, then the centre and Q.
#[derive(Clone, Copy, Debug)]
pub struct DynamicEqSet;

control_set! {
    DynamicEqSet(_s): DynamicEqControls = DynamicEqControls::DEFAULT;
    live: (all);
    Threshold => Db = threshold { whole },
    Knee => Db = knee { whole },
    Ratio => CompressionRatio = ratio { whole },
    Attack => Seconds = attack { whole },
    Release => Seconds = release { whole },
    Range => Db = range { whole },
    // The bell's and the detector's coefficients ramp in the bank, over one grid period.
    Cutoff => Hz = center { whole },
    Q => Q = q { whole };

    fn defaults(&self) -> DynamicEqControls {
        DynamicEqControls::DEFAULT
    }
}

/// The band as a [`Kernel`]: a [`DynamicEqBank`] sized at the prepared rate, its detector on
/// the [`KeySource`] it was built with.
#[derive(Clone, Debug)]
pub struct DynamicEqKernel<F: Float + SimdLanes> {
    input: BusTopology,
    key: KeySource,
    mode: DynamicMode,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<DynamicEqBank<F>>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float + SimdLanes + Lerp> DynamicEqKernel<F> {
    fn new(input: BusTopology, key: KeySource, mode: DynamicMode) -> Self {
        Self {
            input,
            key,
            mode,
            bank: None,
            applied: false,
        }
    }
}

impl<F: Float + SimdLanes + Lerp> Kernel for DynamicEqKernel<F> {
    type Float = F;
    type Set = DynamicEqSet;

    /// A cell that moves reaches the render within 16 frames, wherever the blocks fall.
    const GRID: usize = COEFF_INTERVAL;

    /// The input bus, then, for a [`KeySource::Sidechain`], the key bus as wide; one output
    /// bus where the input is.
    fn shape(&self) -> Shape {
        let main = Buses::main(self.input);
        let inputs = match self.key {
            KeySource::Own => main,
            KeySource::Sidechain => main
                .with(Bus::new(self.input, BusRole::Sidechain))
                .expect("a main bus and a key are two buses"),
        };
        Shape::buses(inputs, Buses::main(self.input)).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = Some(DynamicEqBank::new(self.input.width(), rate).mode(self.mode));
        self.applied = false;
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &DynamicEqControls) {
        let Some(bank) = &mut self.bank else { return };
        bank.retarget(controls);
        if !self.applied {
            bank.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        signals: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let input = block.input();
        self.render_keyed(block, input, signals)
    }

    fn render_keyed(
        &mut self,
        block: &mut InputOutput<'_, F>,
        key: Planes<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        match &mut self.bank {
            Some(bank) => bank.process(key, block),
            None => Ok(()),
        }
    }

    /// A bell rings for as long as its Q says, which is too dear to find per block: unknown,
    /// so the graph's tail fold never reads it as none.
    fn tail(&self) -> Tail {
        Tail::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::InputOutput;
    use tutti_fixtures::signal::wide_signal;
    use tutti_units::Samples;

    /// An `f64` kernel keeps the bell in `f64`: with a range of 0 dB the band never leaves
    /// flat, so a signal of more than 24 bits comes out exactly as it went in (an `f32` state
    /// narrows it by 1e-8).
    #[test]
    fn an_f64_kernel_with_zero_range_returns_a_wide_signal_unchanged() {
        let controls = DynamicEqControls {
            threshold: Db::new(-60.0),
            ratio: CompressionRatio::new(4.0),
            range: Db::new(0.0),
            ..DynamicEqControls::DEFAULT
        };
        let mut k =
            DynamicEqKernel::<f64>::new(BusTopology::MONO, KeySource::Own, DynamicMode::Downward);
        k.prepare(
            SampleRate::SR_48K,
            MaxBlock::of(tutti_units::Samples::new(2_000)),
        );
        k.retarget(&controls);
        let x = wide_signal(2_000);
        let mut y = vec![0.0f64; x.len()];
        k.render(
            &mut InputOutput::from_slices(Samples::new(x.len()), &[&x[..]], &mut [&mut y[..]])
                .unwrap(),
            ControlSignals::NONE,
        )
        .unwrap();
        assert_eq!(y, x);
    }
}
