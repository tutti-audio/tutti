//! [`LevellerNode`]: a real-time loudness leveller, an automatic gain control on the
//! short-term loudness.

use tutti_buffers::{BusTopology, ChannelTopology, MaxBlock};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{
    COEFF_INTERVAL, ChannelWeight, ChannelWeightError, LevellerBank, LevellerControls, SimdLanes,
    channel_weights,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_units::{Db, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`LevellerNode`] is made from: the bus it levels and the controls it starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevellerConfig {
    input: BusTopology,
    controls: LevellerControls,
}

impl LevellerConfig {
    /// A leveller of the bus `input`, whose speaker placement weighs its channels, at
    /// [`LevellerControls::DEFAULT`]: -23 LUFS, up to 12 dB of boost and of cut, a gate at
    /// -50 LUFS, a 0.5 s attack and a 2 s release.
    #[must_use]
    pub fn new(input: BusTopology) -> Self {
        Self {
            input,
            controls: LevellerControls::DEFAULT,
        }
    }

    /// The short-term loudness the output is moved to, in LUFS, at the start.
    #[must_use]
    pub fn target(mut self, target: Db) -> Self {
        self.controls.target = target;
        self
    }

    /// The most the gain adds, in dB, at the start; read within
    /// [`LevellerControls::MAX_RANGE`].
    #[must_use]
    pub fn max_boost(mut self, boost: Db) -> Self {
        self.controls.max_boost = boost;
        self
    }

    /// The most the gain removes, in dB, at the start; read as the boost is.
    #[must_use]
    pub fn max_cut(mut self, cut: Db) -> Self {
        self.controls.max_cut = cut;
        self
    }

    /// The short-term loudness, in LUFS, under which the gain does not rise, at the start.
    #[must_use]
    pub fn gate(mut self, gate: Db) -> Self {
        self.controls.gate = gate;
        self
    }

    /// How fast the gain falls, as a time constant, at the start. Below 0 reads as 0.
    #[must_use]
    pub fn attack(mut self, time: Seconds) -> Self {
        self.controls.attack = time;
        self
    }

    /// How fast the gain rises, as a time constant, at the start. Below 0 reads as 0.
    #[must_use]
    pub fn release(mut self, time: Seconds) -> Self {
        self.controls.release = time;
        self
    }
}

/// A loudness leveller of any width up to one bus (16 channels): one gain, from the short-term
/// loudness of the whole signal, applied to every channel.
///
/// The loudness is BS.1770's: each channel through the K-weighting, the mean squares summed
/// with the weight of the speaker's placement (the bus's own: a 5.1's side speakers weigh
/// 1.41 and its LFE nothing), over the last 3 s (EBU Tech 3341's short-term window) updated
/// every 100 ms. The gain moves that loudness to the target, within the boost and the cut,
/// and holds where the loudness is under the gate (silence is not brought up). See
/// [`LevellerBank`] for the measurement and the gain.
///
/// The six controls are live cells on the stream grid (16 frames): the target and the gate
/// are [`UnitParam::Threshold`](tutti_units::UnitParam::Threshold) at index 0 and 1 ([`Db`],
/// LUFS), the boost and the cut [`UnitParam::Range`](tutti_units::UnitParam::Range) at index 0
/// and 1 ([`Db`]), and the times [`UnitParam::Attack`](tutti_units::UnitParam::Attack) and
/// [`UnitParam::Release`](tutti_units::UnitParam::Release) ([`Seconds`]).
///
/// No latency and no tail: the gain only scales, so a silent input is a silent output. A
/// [`KernelNode`]; made from a [`LevellerConfig`] (`LevellerNode::try_from`), which refuses a
/// bus with no speaker placement to weigh.
///
/// `F` is the float of the graph it runs in, and of the K-weighting and the gain.
pub type LevellerNode<F> = KernelNode<LevellerKernel<F>>;

impl<F: Float + SimdLanes> TryFrom<LevellerConfig> for LevellerNode<F> {
    type Error = ChannelWeightError;

    /// # Errors
    ///
    /// [`ChannelWeightError`] for an ambisonic bus and for speakers that are not a bed's.
    fn try_from(config: LevellerConfig) -> Result<Self, ChannelWeightError> {
        let weights = channel_weights(&ChannelTopology::from(config.input))?;
        Ok(Self::with_kernel(
            LevellerKernel::new(config.input, weights),
            LevellerSet,
            config.controls,
        ))
    }
}

/// The leveller's controls: the target and the gate, the boost and the cut, and the two times.
#[derive(Clone, Copy, Debug)]
pub struct LevellerSet;

control_set! {
    LevellerSet(_s): LevellerControls = LevellerControls::DEFAULT;
    live: (all);
    Threshold[0] => Db = target { whole },
    Threshold[1] => Db = gate { whole },
    Range[0] => Db = max_boost { whole },
    Range[1] => Db = max_cut { whole },
    Attack => Seconds = attack { whole },
    Release => Seconds = release { whole };

    fn defaults(&self) -> LevellerControls {
        LevellerControls::DEFAULT
    }
}

/// The leveller as a [`Kernel`]: a [`LevellerBank`] at the prepared rate over the bus's weights.
#[derive(Clone, Debug)]
pub struct LevellerKernel<F: Float + SimdLanes> {
    input: BusTopology,
    weights: Box<[ChannelWeight]>,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<LevellerBank<F>>,
}

impl<F: Float + SimdLanes> LevellerKernel<F> {
    fn new(input: BusTopology, weights: Box<[ChannelWeight]>) -> Self {
        Self {
            input,
            weights,
            bank: None,
        }
    }
}

impl<F: Float + SimdLanes> Kernel for LevellerKernel<F> {
    type Float = F;
    type Set = LevellerSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        let output = Bus::banded(self.input, BusRole::Main, 1).expect("one band is a bus");
        Shape::buses(
            Buses::main(self.input),
            Buses::NONE.with(output).expect("one bus"),
        )
        .with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = Some(LevellerBank::new(rate, &self.weights));
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &LevellerControls) {
        if let Some(bank) = &mut self.bank {
            bank.retarget(controls);
        }
    }

    fn render(
        &mut self,
        input: Planes<'_, F>,
        _: ControlSignals<'_, F>,
        output: PlanesMut<'_, '_, F>,
    ) {
        if let Some(bank) = &mut self.bank {
            bank.process(input, output);
        }
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}
