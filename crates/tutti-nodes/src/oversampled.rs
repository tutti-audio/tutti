//! [`Oversampled`]: the oversampling a nonlinear kernel runs its shaping inside.

use tutti_buffers::BlockSide;
use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_buffers::{InputOutput, MAX_CHANNELS, MaxBlock};
use tutti_dsp::{
    BlockMismatch, Lerp, Oversampler, Oversampling, OversamplingFactor, OversamplingPlan,
};
use tutti_units::{SampleRate, Samples};

/// The oversampling of a prepared kernel: one [`Oversampler`] per channel and a control signal
/// held across the oversampled frames.
#[derive(Clone, Debug)]
pub(crate) struct Oversampled<F: Float> {
    pub(crate) plan: OversamplingPlan,
    pub(crate) channels: Vec<Oversampler<F>>,
    /// A control signal at the oversampled rate.
    signal: Vec<F>,
    max_block: usize,
}

impl<F: Float + Lerp> Oversampled<F> {
    /// The oversampling by `factor` of `width` channels at `rate`, or `None` for a rate the
    /// default half-band cannot be designed at (one no device runs at).
    pub(crate) fn new(
        factor: OversamplingFactor,
        rate: SampleRate,
        max_block: MaxBlock,
        width: usize,
    ) -> Option<Self> {
        let plan = Oversampling::new(factor).plan(rate).ok()?;
        let state = plan.state::<F>(max_block);
        Some(Self {
            signal: vec![F::ZERO; max_block.get() * factor.get()],
            channels: vec![state; width],
            plan,
            max_block: max_block.get(),
        })
    }

    /// `block` through `shape` at the oversampled rate, in blocks of at most the prepared
    /// maximum. `shape` takes the control signal held at that rate (`signal` per frame, NaN
    /// past its end) and the oversampled block, as wide as the block.
    pub(crate) fn render<S: Float>(
        &mut self,
        signal: Option<Mono<'_, S>>,
        block: &mut InputOutput<'_, S>,
        mut shape: impl FnMut(Option<Mono<'_, F>>, &mut InputOutput<'_, F>) -> Result<(), BlockMismatch>,
    ) -> Result<(), BlockMismatch> {
        let width = self.channels.len();
        for (side, found) in [
            (BlockSide::Input, block.input().channels()),
            (BlockSide::Output, block.output().channels()),
        ] {
            if found != width {
                return Err(BlockMismatch::Width {
                    side,
                    expected: width,
                    found,
                });
            }
        }
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let factor = self.plan.factor().get();
        let mut start = 0;
        while width > 0 && start < frames {
            let end = (start + self.max_block).min(frames);
            let span = Samples::new(start)..Samples::new(end);
            for (c, state) in self.channels.iter_mut().enumerate().take(width) {
                // The span is at most `self.max_block`, which the channel states are built for.
                state
                    .upsample(input.plane(c).slice(span.clone()))
                    .expect("a span is at most the states' max block");
            }
            let held = signal.map(|l| {
                let high = &mut self.signal[..(end - start) * factor];
                for (k, h) in high.iter_mut().enumerate() {
                    // A frame past the control signal's end, like a non-finite one, reads the control in force.
                    *h = l
                        .samples()
                        .get(start + k / factor)
                        .map_or(F::NAN, |d| d.convert_to::<F>());
                }
                Mono::new(&*high)
            });
            let mut highs: [&[F]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            let mut shaped: [&mut [F]; MAX_CHANNELS] = core::array::from_fn(|_| &mut [][..]);
            for ((h, s), state) in highs.iter_mut().zip(&mut shaped).zip(&mut self.channels) {
                let (high, out) = state.high_rate();
                (*h, *s) = (high.samples(), out.into_samples_mut());
            }
            if let (Ok(high), Ok(shaped)) = (
                Planes::new(&highs[..width]),
                PlanesMut::new(&mut shaped[..width]),
            ) && let Ok(mut wide) = InputOutput::new(high.frames(), high, shaped)
            {
                let done = shape(held, &mut wide);
                debug_assert!(done.is_ok(), "the shaper is the node's width");
            }
            for (c, state) in self.channels.iter_mut().enumerate().take(width) {
                state
                    .downsample(output.plane_mut(c).slice_mut(span.clone()))
                    .expect("a span is at most the states' max block");
            }
            start = end;
        }
        Ok(())
    }
}
