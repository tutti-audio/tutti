//! [`TransientShaperNode`]: raises or lowers a signal's attacks and its sustain apart.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, Lerp, TransientBank, TransientControls};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

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
            fast: Seconds::new(0.001),
            slow: Seconds::new(0.03),
        }
    }

    /// The most the gain adds while the signal's attack is on (the fast envelope above the slow
    /// one), at the start; read within [`TransientControls::MAX_AMOUNT`] either way.
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
/// attack and index 1 the sustain, each read within [`TransientControls::MAX_AMOUNT`]. They are
/// sampled on the stream grid (16 frames) and ramp over [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP).
/// The two envelope times are the build's.
///
/// No latency and no tail: the gain only scales, so a silent input is a silent output. A
/// [`KernelNode`]; made from a [`TransientShaperConfig`] (`TransientShaperNode::from`).
///
/// `F` is the float of the graph it runs in, and of the envelopes and the gain.
///
/// # Panics
///
/// `TransientShaperNode::from` panics past 16 channels: a shaper is one bus.
pub type TransientShaperNode<F> = KernelNode<TransientShaperKernel<F>>;

impl<F: Float + Lerp> From<TransientShaperConfig> for TransientShaperNode<F> {
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
pub struct TransientShaperKernel<F: Float + Lerp> {
    input: ChannelTopology,
    fast: Seconds,
    slow: Seconds,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<TransientBank<F>>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float + Lerp> TransientShaperKernel<F> {
    fn new(config: &TransientShaperConfig) -> Self {
        Self {
            input: ChannelTopology::default_for(config.width)
                .expect("a transient shaper is at most 16 channels wide"),
            fast: config.fast,
            slow: config.slow,
            bank: None,
            applied: false,
        }
    }
}

impl<F: Float + Lerp> Kernel for TransientShaperKernel<F> {
    type Float = F;
    type Set = TransientShaperSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = Some(TransientBank::new(rate, self.fast, self.slow));
        self.applied = false;
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &TransientControls) {
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
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        match &mut self.bank {
            Some(bank) => bank.process(block),
            None => Ok(()),
        }
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::InputOutput;
    use tutti_fixtures::signal::wide_signal;
    use tutti_units::Samples;

    /// An `f64` kernel keeps its envelopes and gain in `f64`: a constant `c` from silence has
    /// `f_n = c (1 - af^n)` and `s_n = c (1 - as^n)` with `a = exp(-1 / (tau rate))`, so at an
    /// attack of `A` dB (and 0 sustain) frame `n` is `c 10^(A (1 - s_n / f_n) / 20)`, to 1e-12
    /// (an `f32` state is off by 1e-7).
    #[test]
    fn an_f64_kernel_follows_the_step_onset_closed_form_in_f64() {
        let rate = SampleRate::SR_48K;
        let config = TransientShaperConfig::new(ChannelLayout::MONO)
            .attack(Db::new(12.0))
            .fast(Seconds::new(0.002))
            .slow(Seconds::new(0.02));
        let mut k = TransientShaperKernel::<f64>::new(&config);
        k.prepare(rate, MaxBlock::of(tutti_units::Samples::new(2_000)));
        k.retarget(&config.controls);
        let c = wide_signal(1)[0];
        let x = vec![c; 2_000];
        let mut y = vec![0.0f64; x.len()];
        k.render(
            &mut InputOutput::from_slices(Samples::new(x.len()), &[&x[..]], &mut [&mut y[..]])
                .unwrap(),
            ControlSignals::NONE,
        )
        .unwrap();
        let pole = |tau: f64| (-1.0 / (tau * rate.get())).exp();
        let (af, asl) = (pole(0.002), pole(0.02));
        for (i, &got) in y.iter().enumerate() {
            let n = (i + 1) as i32;
            let (f, s) = (1.0 - af.powi(n), 1.0 - asl.powi(n));
            let want = c * 10f64.powf(12.0 * (1.0 - s / f) / 20.0);
            assert!(
                (got - want).abs() < 1e-12,
                "frame {i}: {got} against {want}"
            );
        }
    }
}
