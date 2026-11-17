//! Offline analysis as a job: [`Session::analyze`] folds a wave or a file
//! through any [`Measure`]'s meter on a worker and lands the meter's output.
//!
//! The audio states the rate and the width, so the caller states only the
//! analysis: the meter's [`Context`](Measure::Context) is built from them
//! ([`AudioContext`]) and each chunk is shown to the meter in the view it
//! takes ([`AudioInput`]). A file is decoded one chunk at a time and never
//! held whole; a wave in memory is cut into the same chunks, so the two tiers
//! land the same output.

mod feed;
#[cfg(test)]
mod tests;

use std::marker::PhantomData;
use std::sync::Arc;

use tutti_codec::Wave;
use tutti_measure::{Measure, Meter};
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate};

pub use feed::{AudioContext, AudioInput};
use feed::{Bind, Chunk, View};

use crate::job::{Cancel, sealed};
use crate::{AppKey, Clock, Job, JobKind, Session, WaitError, WorkerError};

/// Frames decoded, and pushed to the meter, at a time.
const CHUNK_FRAMES: usize = 16_384;

/// The audio an analysis reads: a wave in memory or a file on disk.
///
/// Converts from what the session already holds: an [`Arc<Wave>`](Wave) or a
/// [`Wave`], and, with a decoder, a path or a pooled
/// `AudioEntry`. A path, and a streamed entry, are decoded
/// chunk by chunk on the job's worker.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Audio {
    /// A wave in memory.
    Wave(Arc<Wave>),
    /// A file, decoded on the worker as the analysis folds it.
    #[cfg(codec)]
    File(std::path::PathBuf),
}

impl From<Wave> for Audio {
    fn from(wave: Wave) -> Self {
        Self::Wave(Arc::new(wave))
    }
}

impl From<Arc<Wave>> for Audio {
    fn from(wave: Arc<Wave>) -> Self {
        Self::Wave(wave)
    }
}

#[cfg(codec)]
impl From<std::path::PathBuf> for Audio {
    fn from(path: std::path::PathBuf) -> Self {
        Self::File(path)
    }
}

#[cfg(codec)]
impl From<&std::path::Path> for Audio {
    fn from(path: &std::path::Path) -> Self {
        Self::File(path.to_path_buf())
    }
}

#[cfg(all(feature = "sampler", codec))]
impl From<crate::AudioEntry> for Audio {
    fn from(entry: crate::AudioEntry) -> Self {
        match entry {
            crate::AudioEntry::Resident(wave) => Self::Wave(wave),
            crate::AudioEntry::Streamed(info) => Self::File(info.path().to_path_buf()),
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
    /// The analysis reads two channels and the audio is of another width.
    #[error("the analysis reads {needs:?} audio, and the audio is {found:?}")]
    Width {
        /// The width the analysis reads.
        needs: ChannelLayout,
        /// The audio's width.
        found: ChannelLayout,
    },
    /// The audio's channels cannot be folded to one for a mono analysis.
    #[error("the audio's channels cannot be folded to mono: {0}")]
    Fold(#[from] tutti_buffers::FoldError),
    /// A chunk was not a whole number of frames of the audio's width.
    #[error("a chunk of the audio is malformed: {0}")]
    Buffer(#[from] tutti_buffers::BufferError),
    /// The worker did not finish.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// The wait could not run ([`Job::wait`]).
    #[error(transparent)]
    Wait(#[from] WaitError),
}

/// The kind of [`Session::analyze`]: the job lands the meter's
/// [`Output`](Meter::Output) over the whole audio.
#[derive(Debug)]
pub struct Analyze<A>(PhantomData<fn() -> A>);

impl<A: Measure + 'static> sealed::Sealed for Analyze<A> where
    <A::Meter as Meter>::Output: Send + 'static
{
}

impl<A: Measure + 'static> JobKind for Analyze<A>
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
    /// The analysis is generic, never boxed: [`Meter`] is not object-safe.
    ///
    /// # Errors
    ///
    /// The job fails with [`AnalyzeError::Measure`] when the meter refuses the
    /// audio or a chunk, `AnalyzeError::Decode` when a file cannot be
    /// decoded, and [`AnalyzeError::Width`] when a stereo analysis is given
    /// audio of another width.
    pub fn analyze<A>(&mut self, audio: impl Into<Audio>, analysis: A) -> Job<Analyze<A>>
    where
        A: Measure + Send + 'static,
        A::Context: AudioContext,
        <A::Meter as Meter>::Output: Send + 'static,
        for<'a> <A::Meter as Meter>::Input<'a>: AudioInput<'a>,
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
enum Reader {
    Wave {
        wave: Arc<Wave>,
        at: usize,
    },
    #[cfg(codec)]
    File(Box<tutti_codec::FileIn>),
}

impl Reader {
    fn open(audio: &Audio) -> Result<Self, AnalyzeError> {
        Ok(match audio {
            Audio::Wave(wave) => Self::Wave {
                wave: Arc::clone(wave),
                at: 0,
            },
            #[cfg(codec)]
            Audio::File(path) => Self::File(Box::new(tutti_codec::FileIn::open(path)?)),
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
    fn next(&mut self, chunk: &mut Chunk) -> Result<(), AnalyzeError> {
        match self {
            Self::Wave { wave, at } => {
                let frames = CHUNK_FRAMES.min(wave.len().get() - *at);
                let width = wave.channels();
                let out = chunk.fill(frames);
                for channel in 0..width {
                    let plane = &wave.channel(channel)[*at..*at + frames];
                    for (frame, &sample) in plane.iter().enumerate() {
                        out[frame * width + channel] = sample;
                    }
                }
                *at += frames;
            }
            #[cfg(codec)]
            Self::File(file) => {
                let filled = file.fill_sequential_interleaved(chunk.fill(CHUNK_FRAMES))?;
                chunk.truncate(filled.get());
            }
        }
        Ok(())
    }
}

/// The worker's whole job: bind, fold every chunk, finish.
fn run<A>(
    audio: &Audio,
    analysis: &A,
    cancel: &Cancel,
) -> Result<<A::Meter as Meter>::Output, AnalyzeError>
where
    A: Measure,
    A::Context: AudioContext,
    for<'a> <A::Meter as Meter>::Input<'a>: AudioInput<'a>,
{
    let mut reader = Reader::open(audio)?;
    let topology = reader.topology();
    let context = A::Context::from_audio(reader.sample_rate(), topology.width());
    let mut meter = analysis.measure(context)?;
    let mut chunk = Chunk::new(&topology);
    loop {
        // A dropped job reads nothing more: its result is discarded.
        if cancel.is_cancelled() {
            return Err(WorkerError::Dropped.into());
        }
        reader.next(&mut chunk)?;
        if chunk.is_empty() {
            return Ok(meter.finish());
        }
        meter.push(<A::Meter as Meter>::Input::view(&mut chunk)?, &mut Discard)?;
    }
}

/// The sink for the items a push yields: the job lands the output alone.
struct Discard;

impl<T> Extend<T> for Discard {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        items.into_iter().for_each(drop);
    }
}
