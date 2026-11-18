//! [`TransientShaperNode`]: raises or lowers a signal's attacks and its sustain apart.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{COEFF_INTERVAL, TransientBank, TransientControls};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_types::{BusTopology, ChannelLayout, MaxBlock};
use tutti_units::{Db, SampleRate, Seconds, Tail};

use crate::kernel::{ControlLanes, Kernel, KernelNode};

/// What a [`TransientShaperNode`] is made from: its width, the amounts it starts at and the
/// time constants of its two envelopes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransientShaperConfig {
    width: ChannelLayout,
    controls: TransientControls,
    fast: Seconds,
    slow: Seconds,
}

impl TransientShaperConfig {
    /// A shaper of `width` channels at 0 dB on both amounts, with a 1 ms fast envelope and a
    /// 30 ms slow one.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            controls: TransientControls::DEFAULT,
            fast: Seconds(0.001),
            slow: Seconds(0.03),
        }
    }

    /// The most the gain adds while the signal's attack is on (the fast envelope above the slow
    /// one), at the start; read within [`TransientBank::MAX_AMOUNT`] either way.
    #[must_use]
    pub fn attack(mut self, amount: Db) -> Self {
        self.controls.attack = amount;
        self
    }

    /// The most the gain adds while the signal decays (the fast envelope below the slow one),
    /// at the start; read as the attack amount is.
    #[must_use]
    pub fn sustain(mut self, amount: Db) -> Self {
        self.controls.sustain = amount;
        self
    }

    /// The time constant of the fast envelope. Below 0 reads as 0 (instant).
    #[must_use]
    pub fn fast(self, time: Seconds) -> Self {
        Self { fast: time, ..self }
    }

    /// The time constant of the slow envelope. Below 0 reads as 0 (instant).
    #[must_use]
    pub fn slow(self, time: Seconds) -> Self {
        Self { slow: time, ..self }
    }
}

/// A transient shaper of any width: one gain from the loudest channel's fast and slow
/// envelopes, applied to every channel, which raises or lowers the attacks and the sustain
/// of the signal apart.
///
/// With `f` the fast envelope and `s` the slow one, the gain in dB is `attack * (1 - s / f)`
/// while `f > s`, `sustain * (1 - f / s)` while `f < s` and 0 where they meet
/// ([`TransientBank`] gives the closed form of a step). The gain is **linked**: one gain from
/// the loudest channel, so the stereo image does not move.
///
/// The two amounts are live [`Db`] cells, both [`UnitParam::GainDb`](tutti_units::UnitParam::GainDb): index 0 is the
/// attack and index 1 the sustain, each read within [`TransientBank::MAX_AMOUNT`]. They are
/// sampled on the stream grid (16 frames) and glide over [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE).
/// The two envelope times are the build's.
///
/// No latency and no tail: the gain only scales, so a silent input is a silent output. A
/// [`KernelNode`]; made from a [`TransientShaperConfig`] (`TransientShaperNode::from`).
///
/// # Panics
///
/// `TransientShaperNode::from` panics past 16 channels: a shaper is one bus.
pub type TransientShaperNode = KernelNode<TransientShaperKernel>;

impl From<TransientShaperConfig> for TransientShaperNode {
    fn from(config: TransientShaperConfig) -> Self {
        Self::with_kernel(
            TransientShaperKernel::new(&config),
            TransientShaperSet,
            config.controls,
        )
    }
}

/// The shaper's controls: the attack and sustain amounts.
#[derive(Clone, Copy, Debug)]
pub struct TransientShaperSet;

control_set! {
    TransientShaperSet(_s): TransientControls = TransientControls::DEFAULT;
    live: (all);
    GainDb[0] => Db = attack { whole },
    GainDb[1] => Db = sustain { whole };

    fn defaults(&self) -> TransientControls {
        TransientControls::DEFAULT
    }
}

/// The shaper as a [`Kernel`]: a [`TransientBank`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct TransientShaperKernel {
    input: BusTopology,
    fast: Seconds,
    slow: Seconds,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<TransientBank>,
    /// Whether the first retarget after `prepare` has landed.
    landed: bool,
}

impl TransientShaperKernel {
    fn new(config: &TransientShaperConfig) -> Self {
        Self {
            input: BusTopology::assumed(config.width)
                .expect("a transient shaper is at most 16 channels wide"),
            fast: config.fast,
            slow: config.slow,
            bank: None,
            landed: false,
        }
    }
}

impl Kernel for TransientShaperKernel {
    type Set = TransientShaperSet;

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
        self.bank = Some(TransientBank::new(rate, self.fast, self.slow));
        self.landed = false;
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &TransientControls) {
        let Some(bank) = &mut self.bank else { return };
        bank.aim(controls);
        if !self.landed {
            bank.settle();
            self.landed = true;
        }
    }

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        _: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    ) {
        if let Some(bank) = &mut self.bank {
            bank.process(input, output);
        }
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}
