//! [`CombFilterNode`]: a delay summed with the signal, forward or back.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, CombBank, CombControls, CombDelay, CombForm};
use tutti_graph::{Cost, Shape};
use tutti_units::{Feedback, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};

/// What a [`CombFilterNode`] is made from: its width, its form and delay, and where its
/// coefficient starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CombFilterConfig {
    width: ChannelLayout,
    form: CombForm,
    delay: CombDelay,
    controls: CombControls,
}

impl CombFilterConfig {
    /// A comb of `width` channels of `form` and `delay`, at
    /// [`CombControls::DEFAULT`]. The delay rounds up to whole frames at the prepared rate; a
    /// [`CombDelay`] is above zero, so it is at least one.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, form: CombForm, delay: CombDelay) -> Self {
        Self {
            width: width.into(),
            form,
            delay,
            controls: CombControls::DEFAULT,
        }
    }

    /// The coefficient it starts at.
    #[must_use]
    pub fn gain(mut self, gain: Feedback) -> Self {
        self.controls.gain = gain;
        self
    }
}

/// A comb filter of any width: the input summed with itself `D` frames late (feedforward) or
/// through a loop of `D` frames (feedback) ([`CombBank`]), the same comb on every channel.
///
/// A feedforward comb of gain `g` notches at the odd multiples of `fs / 2D` down to `1 - g`
/// and peaks at the multiples of `fs / D` at `1 + g`; a feedback comb peaks at the multiples
/// of `fs / D` at `1 / (1 - g)`. The delay is fixed at construction, and the coefficient
/// (`UnitParam::Feedback`, a [`Feedback`]) is a live cell that lands whole on the stream grid
/// (16 frames), a feedback comb holding it inside [`Feedback::MAX_STABLE`]. Wet/dry is the
/// graph's blend of any node (`NodeMix`).
///
/// No latency. Its tail is the delay for a feedforward comb and the loop's ring-out to
/// [`TAIL_FLOOR`](tutti_dsp::TAIL_FLOOR) for a feedback one, at the coefficient in force. A
/// [`KernelNode`]. Made from a [`CombFilterConfig`] (`CombFilterNode::from`).
///
/// # Panics
///
/// `CombFilterNode::from` panics past 16 channels, where the node's shape is built: a comb is
/// one bus.
pub type CombFilterNode<F> = KernelNode<CombFilterKernel<F>>;

kernel_config!(CombFilterConfig => CombFilterNode<F> where [F: Float],
    set(_): CombFilterSet = CombFilterSet);

impl<F: Float> From<CombFilterConfig> for CombFilterNode<F> {
    /// The ring is sized at the rate [`Node::prepare`](tutti_graph::Node::prepare) hands it.
    /// Allocates.
    fn from(config: CombFilterConfig) -> Self {
        Self::with_kernel(
            CombFilterKernel::new(config.width, config.form, config.delay),
            config.kernel_set(),
            config.controls,
        )
    }
}

/// The comb's control: the coefficient.
#[derive(Clone, Copy, Debug)]
pub struct CombFilterSet;

control_set! {
    CombFilterSet(_s): CombControls = CombControls::DEFAULT;
    live: (all);
    Feedback => Feedback = gain { whole };

    fn defaults(&self) -> CombControls {
        CombControls::DEFAULT
    }
}

/// The comb as a [`Kernel`]: a [`CombBank`] sized at the prepared rate.
#[derive(Clone, Debug)]
pub struct CombFilterKernel<F: Float> {
    width: ChannelLayout,
    form: CombForm,
    delay: CombDelay,
    rate: SampleRate,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<CombBank<F>>,
}

impl<F: Float> CombFilterKernel<F> {
    fn new(width: ChannelLayout, form: CombForm, delay: CombDelay) -> Self {
        Self {
            width,
            form,
            delay,
            rate: SampleRate::SR_48K,
            bank: None,
        }
    }
}

impl<F: Float> Kernel for CombFilterKernel<F> {
    type Float = F;
    type Set = CombFilterSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Light`: a ring read and a multiply-add per sample.
    fn shape(&self) -> Shape {
        let bus = unassigned_bus(self.width, "a comb");
        Shape::through(bus).cost(Cost::Light)
    }

    /// Sizes the rings at `rate`: the delay rounded up to whole frames (a re-prepare empties
    /// them).
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.bank = Some(CombBank::new(
            self.form,
            self.delay.frames(rate),
            CombControls::DEFAULT.gain,
            self.width,
        ));
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &CombControls) {
        if let Some(bank) = &mut self.bank {
            bank.retarget(controls);
        }
    }

    fn jump_to_target(&mut self) {}

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
        self.bank.as_ref().map_or(Tail::Unknown, |bank| {
            bank.tail(Feedback::MAX_STABLE, self.rate)
        })
    }

    fn tail_of(&mut self, controls: &CombControls) -> Tail {
        self.bank
            .as_ref()
            .map_or(Tail::Unknown, |bank| bank.tail(controls.gain, self.rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::{assert_declared_tail, prepared};
    use tutti_units::Seconds;

    fn delay(secs: f64) -> CombDelay {
        CombDelay::of(Seconds::new(secs))
    }

    /// 1 kHz: a delay of 4 ms is 4 frames.
    const RATE: SampleRate = SampleRate::of(1_000.0);

    fn node(form: CombForm, gain: f64) -> CombFilterNode<f32> {
        CombFilterNode::from(
            CombFilterConfig::new(1u16, form, delay(0.004)).gain(Feedback::new(gain)),
        )
    }

    /// A feedback comb's declared tail covers its ring-out.
    #[test]
    fn a_feedback_comb_declares_its_ring_out() {
        let mut n = prepared(node(CombForm::Feedback, 0.5), RATE, 64);
        assert_declared_tail(&mut n, RATE);
    }
}
