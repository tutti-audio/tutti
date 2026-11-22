//! The two traits every streaming analysis implements: a config that builds a
//! meter, and a meter that folds chunks of one stream.

use tutti_buffers::Sample;

use crate::MeterError;
use crate::convert::Discard;

/// A config that builds a meter for one stream.
///
/// The config describes the analysis and nothing about the audio; the
/// [`Context`](Self::Context) is what binding it to one stream takes (a
/// [`SampleRate`](tutti_units::SampleRate), a
/// [`ChannelLayout`](tutti_buffers::ChannelLayout), a pair of them, or `()`), and
/// the call checks what depends on the config and the context together.
///
/// `S` is the sample type of the audio the meter reads (a consumer bound
/// `A: Measure<S>` takes views of `S`); a meter converts each chunk to `f64`
/// block by block on a block scratch and never copies the whole chunk.
/// What a push allocates is the meter's own doc.
pub trait Measure<S: Sample> {
    /// What binding the config to a stream needs.
    type Context;
    /// The meter this config builds.
    type Meter: StreamMeter;

    /// A meter at the start of one stream bound to `context`.
    ///
    /// # Errors
    ///
    /// The [`MeterError`] of whatever the config cannot hold at
    /// `context`; a config that can always be bound returns `Ok`.
    fn measure(&self, context: Self::Context) -> Result<Self::Meter, MeterError>;
}

/// A meter that folds chunks of one stream into readings.
///
/// The meter counts the frames pushed, so a push takes no position; an
/// [`Item`](Self::Item) carries its own frame where it has one. A push hands
/// what the chunk completed to the caller's `out` and returns only its error
/// (a chunk of a layout the meter was not bound to, a NaN). The
/// [`Output`](Self::Output) is the batch answer over the whole stream, read by
/// [`finish`](Self::finish).
///
/// A meter is not object-safe (its input borrows per call and `push` is
/// generic over `out`): a consumer is generic over `A: Measure<S>`.
pub trait StreamMeter {
    /// One chunk of the stream, a view from `tutti-buffers`: `Mono`,
    /// `Interleaved` or `StereoPlanes`.
    type Input<'a>;
    /// What a push yields: a peak block, a novelty value, a windowed reading.
    type Item;
    /// The answer over the whole stream: the batch result.
    type Output;

    /// Folds the next chunk of the stream, in order, into the meter and
    /// extends `out` with each item the chunk completes.
    ///
    /// # Errors
    ///
    /// The chunk is refused (a layout that is not the meter's, a non-finite
    /// sample); the meter is untouched.
    fn push(
        &mut self,
        input: Self::Input<'_>,
        out: &mut impl Extend<Self::Item>,
    ) -> Result<(), MeterError>;

    /// The answer over everything pushed, consuming the meter.
    #[must_use]
    fn finish(self) -> Self::Output;
}

/// The whole-buffer form of every meter: one meter of `cfg` bound to `context`,
/// fed `input` as a single chunk, then finished. The one-shot entry points are
/// this call, so they and the streaming path cannot drift.
///
/// # Errors
///
/// The [`MeterError`] of [`Measure::measure`] or of the push.
pub(crate) fn measure_buffer<S: Sample, M: Measure<S>>(
    cfg: &M,
    context: M::Context,
    input: <M::Meter as StreamMeter>::Input<'_>,
) -> Result<<M::Meter as StreamMeter>::Output, MeterError> {
    let mut meter = cfg.measure(context)?;
    meter.push(input, &mut Discard)?;
    Ok(meter.finish())
}

/// Drives one meter of `cfg` over a stream of `frames` frames in uneven chunks
/// of 1, 7 and 64 frames and the rest, through [`StreamMeter::push`]; `push` slices
/// the stream to the frame range it is given and pushes that view. Returns
/// every item the pushes yielded, in order, and the output.
#[cfg(test)]
pub(crate) fn fold_uneven<S: Sample, M: Measure<S>>(
    cfg: &M,
    context: M::Context,
    frames: usize,
    mut push: impl FnMut(
        &mut M::Meter,
        core::ops::Range<usize>,
        &mut Vec<<M::Meter as StreamMeter>::Item>,
    ) -> Result<(), MeterError>,
) -> Result<
    (
        Vec<<M::Meter as StreamMeter>::Item>,
        <M::Meter as StreamMeter>::Output,
    ),
    MeterError,
> {
    let mut meter = cfg.measure(context)?;
    let mut items = Vec::new();
    let mut at: usize = 0;
    for len in [1, 7, 64, usize::MAX] {
        let end = at.saturating_add(len).min(frames);
        push(&mut meter, at..end, &mut items)?;
        at = end;
    }
    Ok((items, meter.finish()))
}
