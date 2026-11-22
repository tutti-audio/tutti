//! The session's sampler: a disk stream opened by work the app awaits, and
//! taken back as a value.
//!
//! Reading a file takes as long as the file's header and its first reads, and
//! a task on the session's executor does it: [`Session::open_stream`] cuts the
//! work ([`OpenStream`]), awaiting it reads the file without the session, and
//! [`Session::apply`] puts what it read in service, answering the stream's
//! control side and the reader its voice is built from, in the session's float:
//!
//! ```ignore
//! let opened = session.open_stream("take_1.wav", StreamStart::HEAD)?.await?;
//! let (stream, reader) = session.apply(opened)?;
//! ```
//!
//! What a file is, resident or streamed, is [`Session::load_audio`]'s (with a
//! `tutti-codec` decoder on): a [`LoadedAudio`](crate::LoadedAudio) holds the
//! wave or names the file [`open_stream`](Session::open_stream) opens.

use std::future::IntoFuture;
use std::sync::Arc;
use tutti_platform::{Asset, WeakExecutor};
use tutti_sampler::{AdoptError, StreamHandle, StreamReader, StreamStart};

use crate::LoadError;
use crate::apply::{Cut, Sealed};
use crate::unblocked::Unblocked;
use crate::{AppKey, Clock, Session, SessionFloat};

/// Why a sampler call did not go through.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SamplerError {
    /// The session holds no disk streamer: it was built without one (a
    /// detached session, which only the `testing` feature builds).
    #[error("the session has no disk streamer")]
    NoStreamer,
    /// The streamer is thread work of tutti's, and this target starts none (the web):
    /// only an offline session that opens no stream runs there.
    #[error("{0} needs a thread of tutti's, and this target starts none")]
    NeedsThreads(crate::BackgroundWork),
    /// The disk streamer could not be started ([`SessionBuilder`](crate::SessionBuilder)).
    #[error(transparent)]
    Streamer(#[from] tutti_sampler::StreamerStartError),
}

/// Why a read file did not become a stream ([`Session::apply`] of an
/// [`OpenedStream`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenStreamError<F: SessionFloat> {
    /// The file was read for another session, which cannot serve it here:
    /// open it again from this one.
    #[error("the file was read for another session: open it again")]
    Stale,
    /// The file's channels cannot be placed into the voice's width: a channel
    /// no speaker names, or a narrowing with an unordered end.
    #[error(transparent)]
    Place(#[from] tutti_sampler::PlaceError),
    /// The session's streamer is gone, so nothing would feed the stream.
    #[error("the session's disk streamer is gone")]
    StreamerGone,
    /// The streamer's command queue is full. Nothing was lost: apply this output
    /// again after the next [`update`](Session::update).
    #[error("the session's disk streamer is busy: apply the file again after the next update")]
    Busy(OpenedStream<F>),
}

/// The session's name for what the streamer refused to adopt.
fn adopt_refusal<F: SessionFloat>(cut: Cut, error: AdoptError<F>) -> OpenStreamError<F> {
    match error {
        AdoptError::Placement(error) => OpenStreamError::Place(error),
        AdoptError::Busy(opened) => OpenStreamError::Busy(OpenedStream {
            cut,
            opened: *opened,
        }),
        // `AdoptError` is `non_exhaustive`: what it may yet name is a streamer that
        // cannot take the stream.
        AdoptError::StreamerGone | _ => OpenStreamError::StreamerGone,
    }
}

/// The reading of a file as a disk stream, cut from the session by
/// [`Session::open_stream`]. It is `Send`, and it is a [`Future`] through
/// [`IntoFuture`]: awaiting it reads the file as a task on the session's
/// executor and resolves to an [`OpenedStream`], which the app hands to
/// [`Session::apply`](crate::Session::apply). There is no blocking form.
///
/// It holds the asset and a weak handle to the session's executor, and nothing
/// else of the session. Dropping it unawaited reads nothing.
#[derive(Debug)]
#[must_use = "an open does nothing unless it is awaited"]
pub struct OpenStream<F: SessionFloat> {
    cut: Cut,
    read: tutti_sampler::OpenStream<F>,
    executor: WeakExecutor,
}

impl<F: SessionFloat> OpenStream<F> {
    /// Reads the file: a seeking decoder when the file can seek, else the
    /// whole file decoded. The session is not touched.
    fn run(self) -> Result<OpenedStream<F>, LoadError> {
        Ok(OpenedStream {
            cut: self.cut,
            opened: self
                .read
                .run()
                .map_err(|error| LoadError::Stream(Arc::new(error)))?,
        })
    }
}

impl<F: SessionFloat> IntoFuture for OpenStream<F> {
    type Output = Result<OpenedStream<F>, LoadError>;
    type IntoFuture = OpeningStream<F>;

    fn into_future(self) -> OpeningStream<F> {
        Unblocked::new(self.executor.clone(), self, Self::run)
    }
}

/// A stream's file being read ([`OpenStream`]): resolves to the
/// [`OpenedStream`] once the file is opened.
///
/// A [`Unblocked`]: its first poll starts the work as a task on the session's executor, and
/// `Session::update` is not needed.
///
/// # Errors
///
/// The output is [`LoadError::Stream`] when the file cannot be opened
/// ([`tutti_sampler::OpenError::Io`]) or is not audio this build decodes ([`tutti_sampler::OpenError::Format`]),
/// [`LoadError::Panicked`] when a decoder panicked, and
/// [`LoadError::ShutDown`] when the session is gone.
///
/// # Panics
///
/// When polled after it resolved.
pub type OpeningStream<F> = Unblocked<OpenStream<F>, OpenedStream<F>, LoadError>;

/// A file read for a disk stream (an awaited [`OpenStream`]), which the session takes
/// back with [`Session::apply`](crate::Session::apply): the stream's control
/// side and the reader its voice is built from, in the session's float. The
/// reader is the voice's (`DiskVoice::from_reader`); the stream is the control
/// side, which a `VoiceNode::streaming` or a pool's `add_streaming` takes, so
/// the voice sounds for as long as the node or the pool slot lives. Dropping
/// the stream ends it.
///
/// A session applies only what was read in its own float; the other is a type
/// error:
///
/// ```compile_fail
/// # use tutti_session::{OpenedStream, Session};
/// fn wrong(session: &mut Session, opened: OpenedStream<f64>) {
///     let _ = session.apply(opened); // `Session` is an `f32` session
/// }
/// ```
///
/// Its compiling twin applies what its own float read:
///
/// ```
/// # use tutti_session::{OpenedStream, Session};
/// fn right(session: &mut Session, opened: OpenedStream<f32>) {
///     let _ = session.apply(opened);
/// }
/// ```
#[derive(Debug)]
#[must_use = "a read file is a stream only once the session applies it"]
pub struct OpenedStream<F: SessionFloat> {
    cut: Cut,
    opened: tutti_sampler::OpenedStream<F>,
}

impl<F: SessionFloat> Sealed<F> for OpenedStream<F> {
    type Applied = (StreamHandle<F>, StreamReader<F>);
    type Error = OpenStreamError<F>;

    fn cut(&self) -> &Cut {
        &self.cut
    }

    fn stale() -> OpenStreamError<F> {
        OpenStreamError::Stale
    }

    fn apply_to<K: AppKey, C: Clock<Float = F>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<(StreamHandle<F>, StreamReader<F>), OpenStreamError<F>> {
        let Self { cut, opened } = self;
        let streamer = session
            .sampler
            .streamer
            .as_ref()
            .ok_or(OpenStreamError::StreamerGone)?;
        streamer
            .adopt(opened)
            .map_err(|error| adopt_refusal(cut, error))
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Cuts the reading of `asset` as a disk stream, its reader starting at
    /// `start`: the work the app awaits ([`OpenStream`]) and hands back to
    /// [`apply`](Self::apply). The voice is as wide as the file
    /// (`tutti_sampler::voice_width`). The session is not touched until the
    /// apply, which never waits: a full streamer queue gives the output back in
    /// [`OpenStreamError::Busy`], to apply again after the next
    /// [`update`](Self::update).
    ///
    /// # Errors
    ///
    /// [`SamplerError::NoStreamer`] when the session holds no disk streamer, and
    /// [`SamplerError::NeedsThreads`] where the target starts no thread to run it.
    pub fn open_stream(
        &self,
        asset: impl Into<Asset>,
        start: StreamStart,
    ) -> Result<OpenStream<C::Float>, SamplerError> {
        let streamer = self
            .sampler
            .streamer
            .as_ref()
            .ok_or(if crate::background::THREADS {
                SamplerError::NoStreamer
            } else {
                SamplerError::NeedsThreads(crate::BackgroundWork::Streamer)
            })?;
        Ok(OpenStream {
            cut: Cut::of(self),
            read: streamer.open_stream(asset, start),
            executor: self.background.executor().downgrade(),
        })
    }
}
