//! Offline analysis as a job: [`Session::analyze`] folds a wave or a file
//! through any [`Measure`]'s meter on a worker and lands the meter's output.
//!
//! The audio states the rate and the width, so the caller states only the
//! analysis: the meter's [`Context`](Measure::Context) is built from them
//! ([`AudioContext`]) and each chunk is shown to the meter in the view it
//! takes ([`AudioInput`]). A file is decoded one chunk at a time and never
//! held whole; a wave in memory is cut into the same chunks, so the two tiers
//! land the same output.
//!
//! The same binding serves a live [`Analyzer`]: a tap opened by
//! [`Taps::analyzer`](crate::Taps::analyzer), which binds the meter to the
//! graph's rate and the tap's width and shows it what the tap holds each time
//! the app drains it.

mod feed;
mod live;
#[cfg(test)]
mod tests;

use std::marker::PhantomData;
use std::sync::Arc;

use tutti_buffers::ChannelTopology;
use tutti_buffers::{Float, Wave};
use tutti_measure::{Measure, Meter};
#[cfg(codec)]
use tutti_platform::Asset;
use tutti_units::{SampleRate, Samples};

pub use feed::{AudioContext, AudioInput, BindError};
use feed::{Bind, Chunk, View};
pub use live::{Analyzer, AnalyzerError};

use crate::job::{Cancel, sealed};
use crate::{AppKey, BackgroundError, Clock, Job, JobKind, Session, WaitError};

/// Frames decoded, and pushed to the meter, at a time.
const CHUNK_FRAMES: Samples = Samples::new(16_384);

/// The audio an analysis reads, in the sample format `S` the measure asks for:
/// a wave in memory or an [`Asset`] to decode.
///
/// Converts from what the session already holds: an [`Arc<Wave<S>>`](Wave) or
/// a [`Wave<S>`]; with a decoder, an [`Asset`] or a path; and with a decoder and
/// feature `sampler`, a pooled `AudioEntry`. An asset, and a streamed entry, are decoded
/// chunk by chunk on the job's worker into `S`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Audio<S: Float = f32> {
    /// A wave in memory, read in its own format.
    Wave(Arc<Wave<S>>),
    /// A file or bytes, decoded on the worker as the analysis folds it.
    #[cfg(codec)]
    File(Asset),
}

impl<S: Float> From<Wave<S>> for Audio<S> {
    fn from(wave: Wave<S>) -> Self {
        Self::Wave(Arc::new(wave))
    }
}

impl<S: Float> From<Arc<Wave<S>>> for Audio<S> {
    fn from(wave: Arc<Wave<S>>) -> Self {
        Self::Wave(wave)
    }
}

#[cfg(codec)]
impl<S: Float> From<Asset> for Audio<S> {
    fn from(asset: Asset) -> Self {
        Self::File(asset)
    }
}

#[cfg(codec)]
impl<S: Float> From<std::path::PathBuf> for Audio<S> {
    fn from(path: std::path::PathBuf) -> Self {
        Asset::from(path).into()
    }
}

#[cfg(codec)]
impl<S: Float> From<&std::path::Path> for Audio<S> {
    fn from(path: &std::path::Path) -> Self {
        Asset::from(path).into()
    }
}

#[cfg(all(feature = "sampler", codec))]
impl<S: Float> From<crate::AudioEntry<S>> for Audio<S> {
    fn from(entry: crate::AudioEntry<S>) -> Self {
        match entry {
            crate::AudioEntry::Resident(wave) => Self::Wave(wave),
            crate::AudioEntry::Streamed(info) => Self::File(info.asset().clone()),
        }
    }
}

/// Why an analysis ended without its output.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AnalyzeError {
    /// The meter refused: the analysis cannot be bound to this audio (a band
    /// above its Nyquist rate, say), or a chunk was refused (a NaN, a width
    /// the meter was not built for).
    #[error("the analysis refused the audio: {0}")]
    Measure(#[from] tutti_measure::Error),
    /// The file could not be opened or decoded.
    #[cfg(codec)]
    #[error("the audio file could not be decoded: {0}")]
    Decode(#[from] tutti_codec::DecodeError),
    /// The audio's channels cannot be shown to the meter in the view it takes.
    #[error(transparent)]
    Bind(#[from] BindError),
    /// The worker did not finish.
    #[error(transparent)]
    Worker(#[from] BackgroundError),
    /// The wait could not run ([`Job::wait`]).
    #[error(transparent)]
    Wait(#[from] WaitError),
}

/// The kind of [`Session::analyze`]: the job lands the meter's
/// [`Output`](Meter::Output) over the whole audio.
#[derive(Debug)]
pub struct Analyze<A, S: Float>(PhantomData<fn() -> (A, S)>);

impl<S: Float, A: Measure<S> + 'static> sealed::Sealed for Analyze<A, S> where
    <A::Meter as Meter>::Output: Send + 'static
{
}

impl<S: Float, A: Measure<S> + 'static> JobKind for Analyze<A, S>
where
    <A::Meter as Meter>::Output: Send + 'static,
{
    type Output = <A::Meter as Meter>::Output;
    type Error = AnalyzeError;
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Folds `audio` through the meter `analysis` builds, on a worker, and
    /// lands the meter's output in a later [`update`](Self::update).
    ///
    /// The rate and the width come from the audio: the analysis is bound to
    /// them ([`AudioContext`]), so a config that cannot hold the audio's rate
    /// fails the job. A file is decoded a chunk at a time, so its length is
    /// not held in memory. Dropping the [`Job`] cancels the analysis between
    /// chunks.
    ///
    /// The analysis reads the measure's own format `S` (`A: Measure<S>`): a
    /// [`Wave<S>`] is read through its own planes, and a file is decoded into
    /// `S`, with no detour through another format. `S` is a float format: the
    /// fold of a wide audio to a mono analysis ([`Fold`](tutti_buffers::Fold))
    /// is defined over floats. A path names no format, so a call that passes
    /// one states `S` (`analyze::<f32, _>`).
    ///
    /// The analysis is generic, never boxed: [`Meter`] is not object-safe.
    ///
    /// # Errors
    ///
    /// The job fails with [`AnalyzeError::Measure`] when the meter refuses the
    /// audio or a chunk, `AnalyzeError::Decode` when a file cannot be
    /// decoded, [`AnalyzeError::Bind`] when the audio cannot be shown to the meter
    /// (a stereo analysis of audio of another width, a mono analysis with no
    /// fold from the audio's topology, a chunk that is not a valid view), and [`AnalyzeError::Worker`] or
    /// [`AnalyzeError::Wait`] when the job's worker fails or is dropped.
    pub fn analyze<S, A>(&mut self, audio: impl Into<Audio<S>>, analysis: A) -> Job<Analyze<A, S>>
    where
        S: Float,
        A: Measure<S> + Send + 'static,
        A::Context: AudioContext,
        <A::Meter as Meter>::Output: Send + 'static,
        for<'a> <A::Meter as Meter>::Input<'a>: AudioInput<'a, S>,
    {
        let audio = audio.into();
        self.jobs.submit_job(
            move |cancel| run(&audio, &analysis, cancel),
            |_, done, ticket| {
                ticket.land(match done {
                    Ok(answer) => answer,
                    Err(failed) => Err(failed.into()),
                });
            },
        )
    }
}

/// Where the chunks come from.
enum Reader<'w, S: Float> {
    Wave {
        wave: &'w Wave<S>,
        at: Samples,
    },
    #[cfg(codec)]
    File(Box<tutti_codec::FileIn<S>>),
}

impl<'w, S: Float> Reader<'w, S> {
    fn open(audio: &'w Audio<S>) -> Result<Self, AnalyzeError> {
        Ok(match audio {
            Audio::Wave(wave) => Self::Wave {
                wave,
                at: Samples::ZERO,
            },
            #[cfg(codec)]
            Audio::File(asset) => {
                Self::File(Box::new(tutti_codec::FileIn::<S>::open(asset.clone())?))
            }
        })
    }

    fn sample_rate(&self) -> SampleRate {
        match self {
            Self::Wave { wave, .. } => wave.sample_rate(),
            #[cfg(codec)]
            Self::File(file) => file.sample_rate(),
        }
    }

    fn topology(&self) -> ChannelTopology {
        match self {
            Self::Wave { wave, .. } => wave.topology().clone(),
            #[cfg(codec)]
            Self::File(file) => file.topology().clone(),
        }
    }

    /// Fills `chunk` with the next frames; it is left empty at the end.
    fn next(&mut self, chunk: &mut Chunk<'w, S>) -> Result<(), AnalyzeError> {
        match self {
            Self::Wave { wave, at } => {
                let wave: &'w Wave<S> = wave;
                let frames = CHUNK_FRAMES.min(wave.len().remaining_after(*at));
                let span = at.get()..at.get() + frames.get();
                chunk.hold_planes(
                    (0..wave.channels()).map(|c| &wave.channel(c).samples()[span.clone()]),
                );
                *at = Samples::new(span.end);
            }
            #[cfg(codec)]
            Self::File(file) => {
                let filled = file.fill_sequential_interleaved(chunk.fill(CHUNK_FRAMES)?)?;
                chunk.truncate(filled);
            }
        }
        Ok(())
    }
}

/// The worker's whole job: bind, fold every chunk, finish.
fn run<S, A>(
    audio: &Audio<S>,
    analysis: &A,
    cancel: &Cancel,
) -> Result<<A::Meter as Meter>::Output, AnalyzeError>
where
    S: Float,
    A: Measure<S>,
    A::Context: AudioContext,
    for<'a> <A::Meter as Meter>::Input<'a>: AudioInput<'a, S>,
{
    let mut reader = Reader::open(audio)?;
    let topology = reader.topology();
    let context = A::Context::from_audio(reader.sample_rate(), topology.width());
    let mut meter = analysis.measure(context)?;
    let mut chunk = Chunk::new(&topology);
    loop {
        // A dropped job reads nothing more: its result is discarded.
        if cancel.is_cancelled() {
            return Err(BackgroundError::Dropped.into());
        }
        reader.next(&mut chunk)?;
        if chunk.is_empty() {
            return Ok(meter.finish());
        }
        meter.push(
            <<A::Meter as Meter>::Input<'_> as View<'_, S>>::view(&mut chunk)?,
            &mut Discard,
        )?;
    }
}

/// The sink for the items a push yields: the job lands the output alone.
struct Discard;

impl<T> Extend<T> for Discard {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        items.into_iter().for_each(drop);
    }
}
