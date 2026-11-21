//! Offline analysis as a value: [`Session::analyze`] returns an [`Analyze`]
//! that, when the app awaits it, folds a wave or a file through any
//! [`Measure`]'s meter and resolves to the meter's output.
//!
//! The audio states the rate and the width, so the caller states only the
//! analysis: the meter's [`Context`](Measure::Context) is built from them
//! ([`AudioContext`]) and each chunk is shown to the meter in the view it
//! takes ([`AudioInput`]). A file is decoded one chunk at a time and never
//! held whole; a wave in memory is cut into the same chunks, so the two tiers
//! return the same output.
//!
//! The same binding serves a live [`Analyzer`]: a tap opened by
//! [`Taps::open_analyzer`](crate::Taps::open_analyzer), which binds the meter to the
//! graph's rate and the tap's width and shows it what the tap holds each time
//! the app drains it.

mod feed;
mod live;
#[cfg(test)]
mod tests;

use std::fmt;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tutti_buffers::ChannelTopology;
use tutti_buffers::{Float, Wave};
use tutti_measure::{Measure, Meter};
#[cfg(codec)]
use tutti_platform::Asset;
use tutti_platform::WeakExecutor;
use tutti_units::{SampleRate, Samples};

pub use feed::{AudioContext, AudioInput, BindError};
use feed::{Bind, Chunk, View};
pub use live::{Analyzer, AnalyzerError};

use crate::unblocked::{Unanswered, Unblocked};
use crate::{AppKey, Clock, Session};

/// Frames decoded, and pushed to the meter, at a time.
const CHUNK_FRAMES: Samples = Samples::new(16_384);

/// The audio an analysis reads, in the sample format `S` the measure asks for:
/// a wave in memory or an [`Asset`] to decode.
///
/// Converts from what the session already holds: an [`Arc<Wave<S>>`](Wave) or
/// a [`Wave<S>`]; with a decoder, an [`Asset`] or a path; and with a decoder and
/// feature `sampler`, a `LoadedAudio`. An asset, and a streamed load, are decoded
/// chunk by chunk into `S` as the analysis runs.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Audio<S: Float = f32> {
    /// A wave in memory, read in its own format.
    Wave(Arc<Wave<S>>),
    /// A file or bytes, decoded as the analysis folds it.
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
impl<S: Float> From<crate::LoadedAudio<S>> for Audio<S> {
    fn from(audio: crate::LoadedAudio<S>) -> Self {
        match audio.data() {
            crate::AudioData::Resident(wave) => Self::Wave(Arc::clone(wave)),
            crate::AudioData::Streamed(_) => Self::File(audio.asset().clone()),
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
    /// The analysis panicked (a decoder, a meter); `message` is the panic's, when
    /// it was a string.
    #[error("the analysis panicked: {message}")]
    Panicked {
        /// The panic's message, or a placeholder for a payload of another type.
        message: String,
    },
    /// The session was dropped before the analysis could run: its executor was
    /// gone, refused the task, or dropped it before it ran. The audio did not fail.
    #[error("the session shut down before the analysis ran")]
    ShutDown,
}

impl Unanswered for AnalyzeError {
    fn shut_down() -> Self {
        Self::ShutDown
    }

    fn panicked(message: String) -> Self {
        Self::Panicked { message }
    }
}

/// An analysis of one audio, built by [`Session::analyze`]. It is `Send` when
/// the analysis is, and it is a [`Future`] through [`IntoFuture`]: awaiting it
/// folds the audio through the meter as a task on the session's executor and
/// resolves to the meter's [`Output`](Meter::Output). There is no blocking
/// form.
///
/// It holds the audio, the analysis and a weak handle to the session's
/// executor, and nothing else of the session. Dropping it unawaited reads
/// nothing.
#[derive(Debug)]
#[must_use = "an analysis does nothing unless it is awaited"]
pub struct Analyze<A, S: Float = f32> {
    audio: Audio<S>,
    analysis: A,
    executor: WeakExecutor,
}

impl<A, S> Analyze<A, S>
where
    S: Float,
    A: Measure<S>,
    A::Context: AudioContext,
    for<'a> <A::Meter as Meter>::Input<'a>: AudioInput<'a, S>,
{
    /// Folds the audio through the meter the analysis builds and returns the
    /// meter's output.
    ///
    /// The cost is a full read of the audio: a file is decoded a chunk at a
    /// time, so its length is not held in memory.
    fn run(self) -> Result<<A::Meter as Meter>::Output, AnalyzeError> {
        let mut reader = Reader::open(&self.audio)?;
        let topology = reader.topology();
        let context = A::Context::from_audio(reader.sample_rate(), topology.width());
        let mut meter = self.analysis.measure(context)?;
        let mut chunk = Chunk::new(&topology);
        loop {
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
}

impl<A, S> IntoFuture for Analyze<A, S>
where
    S: Float,
    A: Measure<S> + Send + 'static,
    A::Context: AudioContext,
    for<'a> <A::Meter as Meter>::Input<'a>: AudioInput<'a, S>,
    <A::Meter as Meter>::Output: Send + 'static,
{
    type Output = Result<<A::Meter as Meter>::Output, AnalyzeError>;
    type IntoFuture = Analyzing<A, S>;

    fn into_future(self) -> Analyzing<A, S> {
        Analyzing {
            analysis: Unblocked::new(self.executor.clone(), self, Self::run),
        }
    }
}

/// An analysis being awaited ([`Analyze`]): resolves to the meter's output
/// once the audio is folded.
///
/// The first poll starts the fold as a task on the session's executor (the
/// session's weak handle, upgraded for the spawn only) and awaits the receiver
/// of a oneshot. **The task fills the sender**: it folds the audio chunk by
/// chunk and sends the output or the error, which wakes the future. A poll never
/// blocks, and `Session::update` is not needed.
///
/// Dropped before it resolves, the future drops the receiver: a fold that has
/// not begun reads nothing, and one in flight finishes and its output is dropped.
/// Dropped before its first poll it starts nothing.
///
/// # Errors
///
/// The output is [`AnalyzeError::Measure`] when the meter refuses the audio or a
/// chunk, `AnalyzeError::Decode` when a file cannot be decoded, and
/// [`AnalyzeError::Bind`] when the audio cannot be shown to the meter (a stereo
/// analysis of audio of another width, a mono analysis with no fold from the
/// audio's topology, a chunk that is not a valid view);
/// [`AnalyzeError::Panicked`] when a decoder or the meter panicked;
/// [`AnalyzeError::ShutDown`] when the session is gone.
///
/// # Panics
///
/// When polled after it resolved.
#[must_use = "an analysis does nothing unless it is awaited"]
pub struct Analyzing<A, S = f32>
where
    S: Float,
    A: Measure<S>,
{
    analysis: Unblocked<Analyze<A, S>, <A::Meter as Meter>::Output, AnalyzeError>,
}

impl<A, S> Future for Analyzing<A, S>
where
    S: Float,
    A: Measure<S> + Send + 'static,
    A::Context: AudioContext,
    for<'a> <A::Meter as Meter>::Input<'a>: AudioInput<'a, S>,
    <A::Meter as Meter>::Output: Send + 'static,
{
    type Output = Result<<A::Meter as Meter>::Output, AnalyzeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.analysis).poll(cx)
    }
}

impl<A, S> fmt::Debug for Analyzing<A, S>
where
    S: Float,
    A: Measure<S>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Analyzing").finish_non_exhaustive()
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// An [`Analyze`] of `audio` through the meter `analysis` builds; nothing
    /// is read until the app awaits it, and the fold costs a full read of the
    /// audio.
    ///
    /// The rate and the width come from the audio: the analysis is bound to
    /// them ([`AudioContext`]), so a config that cannot hold the audio's rate
    /// fails the analysis.
    ///
    /// The analysis reads the measure's own format `S` (`A: Measure<S>`): a
    /// [`Wave<S>`] is read through its own planes, and a file is decoded into
    /// `S`, with no detour through another format. `S` is a float format: the
    /// fold of a wide audio to a mono analysis ([`Fold`](tutti_buffers::Fold))
    /// is defined over floats. A path names no format, so a call that passes
    /// one states `S` (`analyze::<f32, _>`).
    ///
    /// The analysis is generic, never boxed: [`Meter`] is not object-safe.
    pub fn analyze<S, A>(&self, audio: impl Into<Audio<S>>, analysis: A) -> Analyze<A, S>
    where
        S: Float,
        A: Measure<S>,
    {
        Analyze {
            audio: audio.into(),
            analysis,
            executor: self.background.executor().downgrade(),
        }
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

/// The sink for the items a push yields: the run returns the output alone.
struct Discard;

impl<T> Extend<T> for Discard {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        items.into_iter().for_each(drop);
    }
}
