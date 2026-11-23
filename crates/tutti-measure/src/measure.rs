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
///
/// The exception is a whole-buffer analysis (notes, pitch track, tempo, beats,
/// chords, key, alignment): its meter gathers the stream as `f64`, up to the
/// most frames the analysis reads, and runs the analysis at
/// [`finish`](StreamMeter::finish). Its output is a `Result` of the analysis's
/// own error, since what it refuses is known only at the end.
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

/// [`measure_buffer`] for a whole-buffer analysis, whose output is a `Result` of
/// its own error: a refusal of the meter flattens into that error through
/// `From<MeterError>`.
///
/// # Errors
///
/// The analysis's error, from the meter or from the analysis.
pub(crate) fn measure_whole<S, M, T, E>(
    cfg: &M,
    context: M::Context,
    input: <M::Meter as StreamMeter>::Input<'_>,
) -> Result<T, E>
where
    S: Sample,
    M: Measure<S>,
    M::Meter: StreamMeter<Output = Result<T, E>>,
    E: From<MeterError>,
{
    measure_buffer::<S, M>(cfg, context, input)?
}

/// The output of `cfg` over `samples`, read twice: as one chunk and fed in
/// uneven chunks. The two must be equal, which is what makes a meter's
/// reading independent of how the stream is cut; returns the reading.
#[cfg(test)]
pub(crate) fn chunk_independent<M>(
    cfg: &M,
    context: M::Context,
    samples: &[f64],
) -> <M::Meter as StreamMeter>::Output
where
    M: Measure<f64>,
    M::Context: Copy,
    for<'a> M::Meter: StreamMeter<Input<'a> = tutti_buffers::Mono<'a, f64>>,
    <M::Meter as StreamMeter>::Output: PartialEq + core::fmt::Debug,
{
    let whole = measure_buffer::<f64, M>(cfg, context, tutti_buffers::Mono::new(samples))
        .expect("the whole buffer is accepted");
    let (_, chunked) = fold_uneven::<f64, M>(cfg, context, samples.len(), |meter, at, out| {
        meter.push(tutti_buffers::Mono::new(&samples[at]), out)
    })
    .expect("the chunks are accepted");
    assert_eq!(chunked, whole, "the reading depends on the chunking");
    whole
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
