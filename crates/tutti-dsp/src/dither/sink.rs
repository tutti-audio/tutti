//! [`DitherSink`]: the stage that turns a float signal into an integer one, the
//! one path an export and a live recording share.

use tutti_buffers::ChannelLayout;
use tutti_buffers::{
    Float, Interleaved, InterleavedMut, Sample, Sink, SinkFailed, convert_counting_clips,
};
use tutti_units::{Amplitude, Samples};

use crate::NoiseShaping;
use crate::dither::shaping::ErrorFeedback;
use crate::dither::{DitherNoise, DitherShape};

/// Frames dithered and converted at a time: the stage's whole scratch.
///
/// A longer write goes through in pieces of this size. The noise advances once
/// per sample in order, so the split changes nothing in what is written.
const CHUNK: Samples = Samples::new(1024);

/// One least significant bit of the sample format `I` in the `[-1, 1]` float
/// domain, or silence for a float format, which has none.
fn lsb_of<I: Sample>() -> Amplitude {
    I::STEP.map_or(Amplitude::SILENT, Amplitude::new)
}

/// A [`Sink`] of floats `F` in front of a sink of integers `K`: each block is
/// given dither noise one least significant bit of `K`'s format wide
/// ([`Sample::STEP`]), converted to that format by the rule in [`Sample`] and
/// written on, and the values the conversion clipped are counted
/// ([`clipped`](Self::clipped)).
///
/// It computes no noise of its own: [`DitherNoise`] does, seeded as an
/// export's is, one draw per sample in the order the samples arrive, so the
/// sequence is continuous across writes and across channels. With `None` for
/// the shape no noise is added and the conversion alone runs. Frames in are
/// frames out, the layout is the inner sink's, a write the inner sink refuses is
/// refused here ([`SinkFailed`]), and [`finalize`](Sink::finalize) is the inner
/// sink's. A view whose width is not the inner sink's fails the write.
///
/// [`shaped`](Self::shaped) runs the quantizer as error feedback through a
/// [`NoiseShaping`]: the error of each value (against the signal, dither
/// included) is held within two least significant bits and fed through the
/// filter into the next values, so a clip cannot make the loop run away.
///
/// Nothing allocates after [`new`](Self::new) and [`shaped`](Self::shaped): both scratches are one chunk of
/// 1024 frames, sized there. An inner sink of a float format has no step, so
/// no noise is added to it.
///
/// # Examples
///
/// ```
/// use tutti_buffers::{Interleaved, Sink, SinkFailed};
/// use tutti_dsp::{DitherShape, DitherSink};
/// use tutti_buffers::ChannelLayout;
///
/// struct Count(usize);
/// impl Sink for Count {
///     type Sample = i16;
///     type Error = std::convert::Infallible;
///     fn layout(&self) -> ChannelLayout { ChannelLayout::MONO }
///     fn write(&mut self, frames: Interleaved<'_, i16>) -> Result<(), SinkFailed> {
///         self.0 += frames.samples().len();
///         Ok(())
///     }
///     fn finalize(self) -> Result<(), Self::Error> { Ok(()) }
/// }
///
/// let mut sink = DitherSink::<_, f32>::new(Count(0), Some(DitherShape::Triangular));
/// sink.write(Interleaved::new(&[0.0f32; 64], ChannelLayout::MONO).unwrap()).unwrap();
/// assert_eq!(sink.clipped(), 0);
/// assert_eq!(sink.into_inner().0, 64);
/// ```
#[derive(Debug)]
pub struct DitherSink<K: Sink, F> {
    inner: K,
    layout: ChannelLayout,
    noise: DitherNoise,
    shape: Option<DitherShape>,
    lsb: Amplitude,
    shaping: Option<ErrorFeedback<F>>,
    /// The chunk as floats, with its noise.
    noisy: Vec<F>,
    /// The chunk converted to the inner sink's format.
    converted: Vec<K::Sample>,
    clipped: u64,
}

impl<K: Sink, F: Float> DitherSink<K, F> {
    /// Wraps `inner`, adding `shape`'s noise (none for `None`) at one LSB of
    /// `inner`'s format.
    pub fn new(inner: K, shape: Option<DitherShape>) -> Self {
        let layout = inner.layout();
        let room = layout.interleaved_len(CHUNK);
        Self {
            inner,
            layout,
            noise: DitherNoise::new(),
            shape,
            lsb: lsb_of::<K::Sample>(),
            shaping: None,
            noisy: Vec::with_capacity(if shape.is_some() { room } else { 0 }),
            converted: Vec::with_capacity(room),
            clipped: 0,
        }
    }

    /// Runs the quantizer as error feedback through `shaping`. The sink has no
    /// rate of its own, so the caller designs `shaping` for the rate of the
    /// blocks it writes. Allocates its state, one
    /// history of [`order`](NoiseShaping::order) errors per channel.
    #[must_use]
    pub fn shaped(mut self, shaping: &NoiseShaping) -> Self {
        self.shaping = Some(ErrorFeedback::new(
            shaping,
            usize::from(self.layout.count()),
        ));
        self
    }

    /// The values the conversion has clipped so far.
    #[must_use]
    pub fn clipped(&self) -> u64 {
        self.clipped
    }

    /// The wrapped sink, for a caller that finishes it itself.
    pub fn into_inner(self) -> K {
        self.inner
    }

    /// The wrapped sink, to read what it holds between writes.
    pub fn get_mut(&mut self) -> &mut K {
        &mut self.inner
    }
}

impl<K: Sink, F: Float> Sink for DitherSink<K, F> {
    type Sample = F;
    type Error = K::Error;

    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed> {
        if frames.layout() != self.layout {
            return Err(SinkFailed);
        }
        let layout = self.layout;
        let mut at = Samples::ZERO;
        while at < frames.frames() {
            let end = (at + CHUNK).min(frames.frames());
            let part = frames.slice(at..end);
            // Within the capacity `new` reserved: neither grows.
            self.converted.clear();
            self.converted
                .resize(part.samples().len(), K::Sample::SILENCE);
            let converted =
                InterleavedMut::new(&mut self.converted, layout).map_err(|_| SinkFailed)?;
            let mut noisy;
            let dithered = match self.shape {
                Some(shape) => {
                    self.noisy.clear();
                    self.noisy.extend_from_slice(part.samples());
                    noisy = InterleavedMut::new(&mut self.noisy, layout).map_err(|_| SinkFailed)?;
                    self.noise.process(shape, self.lsb, noisy.reborrow());
                    noisy.as_interleaved()
                }
                None => part,
            };
            let clipped = match &mut self.shaping {
                Some(feedback) => feedback.process(self.lsb, part, dithered, converted),
                None => convert_counting_clips(dithered, converted).map_err(|_| SinkFailed)?,
            };
            self.clipped += clipped as u64;
            let block = Interleaved::new(&self.converted, layout).map_err(|_| SinkFailed)?;
            self.inner.write(block)?;
            at = end;
        }
        Ok(())
    }

    fn finalize(self) -> Result<(), Self::Error> {
        self.inner.finalize()
    }
}

#[cfg(test)]
mod tests;
