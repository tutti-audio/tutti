//! [`Quantizer`]: the stage that turns a float signal into an integer one, the
//! one path an export and a live recording share.

use tutti_buffers::{
    ChannelLayout, Float, Interleaved, InterleavedMut, Sample, convert_counting_clips,
};
use tutti_units::{Amplitude, Samples};

use crate::NoiseShaping;
use crate::dither::{DitherNoise, DitherShape};

/// Frames dithered and converted at a time: the stage's whole scratch.
///
/// A longer block goes through in pieces of this size. The noise advances once
/// per sample in order, so the split changes nothing in what is written.
const CHUNK: Samples = Samples::new(1024);

/// One least significant bit of the sample format `I` in the `[-1, 1]` float
/// domain, or silence for a float format, which has none.
fn lsb_of<I: Sample>() -> Amplitude {
    I::STEP.map_or(Amplitude::SILENT, Amplitude::new)
}

/// Floats `F` quantized to a sample format: each block is given dither noise one least
/// significant bit of the target format wide ([`Sample::STEP`]) and converted to that format by
/// the rule in [`Sample`], and the values the conversion clipped are counted.
///
/// The target format `I` of [`quantize`](Self::quantize) is the one grid: its step scales the
/// noise and its conversion rounds, so the two cannot disagree. A float `I` has no step, so
/// no noise is added to it and the conversion alone runs.
///
/// The noise is [`DitherNoise`], one draw per sample in the order the samples arrive, so the
/// sequence is continuous across blocks and across channels. With `None` for the shape no
/// noise is added and the conversion alone runs. Seed each quantizer with
/// [`SplitMix64::mix`](tutti_rt::SplitMix64::mix) of an index, so that the quantizers of
/// signals that are summed later do not share a noise sequence.
///
/// [`shaped`](Self::shaped) runs the quantizer as error feedback through a [`NoiseShaping`]: the
/// error of each value (against the signal, dither included) is held within two least
/// significant bits and fed through the filter into the next values, so a clip cannot make the
/// loop run away.
///
/// Nothing allocates after [`new`](Self::new) and [`shaped`](Self::shaped): the scratch is one
/// chunk of 1024 frames, sized there.
///
/// # Examples
///
/// ```
/// use tutti_buffers::{ChannelLayout, Interleaved, InterleavedMut};
/// use tutti_dsp::{DitherShape, Quantizer};
///
/// let mut quantizer =
///     Quantizer::<f32>::new(Some(DitherShape::Triangular), ChannelLayout::MONO, 1);
/// let silence = [0.0f32; 64];
/// let mut out = [0i16; 64];
/// let clipped = quantizer.quantize(
///     Interleaved::new(&silence, ChannelLayout::MONO).unwrap(),
///     InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap(),
/// );
/// assert_eq!(clipped, 0);
/// assert!(out.iter().all(|v| v.abs() <= 1));
/// ```
#[derive(Debug)]
pub struct Quantizer<F> {
    layout: ChannelLayout,
    noise: DitherNoise,
    shape: Option<DitherShape>,
    shaping: Option<ErrorFeedback<F>>,
    /// The chunk as floats, with its noise.
    noisy: Vec<F>,
}

impl<F: Float> Quantizer<F> {
    /// A quantizer of blocks of `layout`, adding `shape`'s noise (none for `None`) from the
    /// noise sequence of `seed`.
    #[must_use]
    pub fn new(shape: Option<DitherShape>, layout: ChannelLayout, seed: u64) -> Self {
        Self {
            layout,
            noise: DitherNoise::with_seed(seed),
            shape,
            shaping: None,
            noisy: Vec::with_capacity(if shape.is_some() {
                layout.interleaved_len(CHUNK)
            } else {
                0
            }),
        }
    }

    /// Runs the quantizer as error feedback through `shaping`. The quantizer has no rate of its
    /// own, so the caller designs `shaping` for the rate of the blocks it quantizes. Allocates
    /// its state, one history of [`order`](NoiseShaping::order) errors per channel.
    #[must_use]
    pub fn shaped(mut self, shaping: &NoiseShaping) -> Self {
        self.shaping = Some(ErrorFeedback::new(
            shaping,
            usize::from(self.layout.count()),
        ));
        self
    }

    /// Quantizes `input` into `out`, a block of the target format `I`, and returns the values
    /// that clipped in this block.
    ///
    /// # Panics
    ///
    /// If `input` or `out` is not of the quantizer's layout, or their frame counts differ.
    pub fn quantize<I: Sample>(
        &mut self,
        input: Interleaved<'_, F>,
        mut out: InterleavedMut<'_, I>,
    ) -> u64 {
        assert!(
            input.layout() == self.layout
                && out.layout() == self.layout
                && input.frames() == out.frames(),
            "a quantizer's input and output are blocks of its layout and one length"
        );
        let layout = self.layout;
        let lsb = lsb_of::<I>();
        let on_grid = I::STEP.is_some();
        let mut clipped = 0;
        let mut at = Samples::ZERO;
        while at < input.frames() {
            let end = (at + CHUNK).min(input.frames());
            let part = input.slice(at..end);
            let converted = out.slice_mut(at..end);
            let mut noisy;
            let dithered = match self.shape.filter(|_| on_grid) {
                Some(shape) => {
                    self.noisy.clear();
                    self.noisy.extend_from_slice(part.samples());
                    noisy = InterleavedMut::new(&mut self.noisy, layout)
                        .expect("a chunk is whole frames of the layout");
                    self.noise.process(shape, lsb, noisy.reborrow());
                    noisy.as_interleaved()
                }
                None => part,
            };
            let count = match &mut self.shaping {
                Some(feedback) if on_grid => feedback.process(lsb, part, dithered, converted),
                _ => convert_counting_clips(dithered, converted)
                    .expect("the chunk and its output share a shape"),
            };
            clipped += count as u64;
            at = end;
        }
        clipped
    }
}

/// The running error-feedback state of every channel: the last `order`
/// errors of each, sized at construction.
///
/// The fed-back error is held within two least significant bits. With dither up
/// to one LSB in amplitude the error `y - (x + feedback)` is at most 1.5 LSB, so
/// the bound is never reached while the quantizer is in range; it only acts when
/// a value clips, where the error is as large as the overload. The feedback is
/// then at most `2 Σ|c_k|` LSB and the filter forgets the overload after as
/// many frames as it has taps.
#[derive(Debug, Clone)]
pub(crate) struct ErrorFeedback<F> {
    /// `c_n..=c_1`: oldest error first, to match a window of the history.
    taps: Vec<F>,
    /// Per channel, `2 * order` errors: each is written at `head` and at
    /// `head + order`, so `[head, head + order)` holds the last `order`
    /// errors, oldest first, as one slice.
    history: Vec<Vec<F>>,
    head: usize,
}

impl<F: Float> ErrorFeedback<F> {
    pub(crate) fn new(shaping: &NoiseShaping, channels: usize) -> Self {
        let taps: Vec<F> = shaping
            .taps()
            .iter()
            .rev()
            .map(|&c| F::from_design(c))
            .collect();
        Self {
            history: vec![vec![F::ZERO; 2 * taps.len()]; channels],
            taps,
            head: 0,
        }
    }

    /// The largest error held in the history, in float units.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> F {
        self.history
            .iter()
            .flatten()
            .fold(F::ZERO, |m, &e| m.max(e.abs()))
    }

    /// The largest error held for channel `c`, in float units.
    #[cfg(test)]
    pub(crate) fn peak_of(&self, c: usize) -> F {
        self.history[c].iter().fold(F::ZERO, |m, &e| m.max(e.abs()))
    }

    /// Quantizes `dithered` (the signal `clean` with its noise) into `out` by
    /// the rule in [`Sample`], feeding back the error of each value against
    /// `clean`; returns the clips. `lsb` is one step of `S`.
    pub(crate) fn process<S: Sample>(
        &mut self,
        lsb: Amplitude,
        clean: Interleaved<'_, F>,
        dithered: Interleaved<'_, F>,
        mut out: InterleavedMut<'_, S>,
    ) -> usize {
        let order = self.taps.len();
        let bound = F::from_unit(lsb) * F::lit(2.0);
        let mut clipped = 0;
        let frames = clean.iter().zip(dithered.iter()).zip(out.iter_mut());
        for ((clean, dithered), out) in frames {
            let channels = clean.iter().zip(dithered).zip(out).zip(&mut self.history);
            for (((&x, &u), y), history) in channels {
                let window = &history[self.head..self.head + order];
                let feedback = self
                    .taps
                    .iter()
                    .zip(window)
                    .fold(F::ZERO, |acc, (&tap, &e)| acc + tap * e);
                let (value, clip) = (u + feedback).convert_to_clipped::<S>();
                *y = value;
                clipped += usize::from(clip);
                let error = value.convert_to::<F>() - (x + feedback);
                // A non-finite input leaves nothing behind, a clip no more than the bound.
                let error = if error.is_finite() {
                    error.clamp(-bound, bound)
                } else {
                    F::ZERO
                };
                history[self.head] = error;
                history[self.head + order] = error;
            }
            self.head = (self.head + 1) % order;
        }
        clipped
    }
}

#[cfg(test)]
mod tests;
