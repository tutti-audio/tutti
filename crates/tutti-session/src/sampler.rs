//! The session's sampler: a disk stream opened on the session's spawner, and
//! what lands from it in [`Session::update`](crate::Session::update).
//!
//! The streamer opens a file on a thread of its own, so an open takes as long
//! as the file's header and its first reads. [`Session::open_stream`] starts
//! one and returns at once; the stream lands as
//! [`SamplerEvent::Opened`] with the call's [`SamplerLoad`], under
//! [`SessionEvent::Sampler`], or as [`SamplerEvent::OpenFailed`] with the
//! reason.
//!
//! What a file is, resident or streamed, is the pool's: an
//! [`audio_file`](crate::audio_file) entry reads as an
//! [`AudioEntry`](crate::AudioEntry), and a streamed one names the path
//! [`open_stream`](Session::open_stream) opens.

use std::path::PathBuf;

use tutti_sampler::{OpenError, Placement, Stream, StreamReader};

use crate::WorkerError;
use crate::{Session, SessionEvent};

tutti_types::id! {
    /// An open ([`Session::open_stream`]) the session started, named so the event that lands it can be matched to the call:
    /// [`SamplerEvent::Opened`] and [`SamplerEvent::OpenFailed`] carry it. The
    /// session mints it; it names nothing else.
    pub struct SamplerLoad;
}

/// A sampler event of the session, one per open that ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum SamplerEvent {
    /// An open [`Session::open_stream`] started finished: the stream is
    /// running, and its two halves are the host's. The reader is the voice's
    /// (`DiskVoice::from_reader`); the stream is the control side, which a
    /// `VoiceNode::streaming` or a pool's `add_streaming` takes, so the voice
    /// sounds for as long as the node or the pool slot lives. Dropping the
    /// stream ends it.
    Opened {
        /// The id [`Session::open_stream`] returned.
        id: SamplerLoad,
        /// The control side of the stream.
        stream: Stream,
        /// The voice's side of the stream.
        reader: StreamReader,
    },
    /// An open did not finish, and will not.
    OpenFailed {
        /// The id [`Session::open_stream`] returned.
        id: SamplerLoad,
        /// Why.
        error: SamplerError,
    },
}

impl From<SamplerEvent> for SessionEvent {
    fn from(event: SamplerEvent) -> Self {
        Self::Sampler(event)
    }
}

/// Why a sampler call or open did not go through.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SamplerError {
    /// The session holds no disk streamer: it was built without one (a
    /// detached session, which only the `testing` feature builds).
    #[error("the session has no disk streamer")]
    NoStreamer,
    /// The disk streamer could not be started ([`SessionBuilder`](crate::SessionBuilder)).
    #[error(transparent)]
    Streamer(#[from] tutti_sampler::Error),
    /// The streamer could not open the file: it is missing or unreadable, is
    /// not audio this build decodes, cannot be placed into a voice, or the
    /// streamer thread is gone.
    #[error(transparent)]
    Open(#[from] OpenError),
    /// The open's job did not finish: it panicked, or the spawner dropped it.
    #[error(transparent)]
    Job(#[from] WorkerError),
}

impl<C: crate::Clock> Session<C> {
    /// Opens `path` as a disk stream, its reader starting at `place`, on the
    /// session's spawner. Returns at once; the stream lands as
    /// [`SamplerEvent::Opened`] with the returned id in a later
    /// [`update`](Self::update), on the thread that calls `update()`. A file
    /// that cannot be opened lands as [`SamplerEvent::OpenFailed`], a job that
    /// did not finish as [`SamplerError::Job`] in it.
    ///
    /// The voice is as wide as the file (`tutti_sampler::voice_width`). Sending
    /// the open waits while the streamer's command queue is full, as every
    /// stream command does.
    ///
    /// # Errors
    ///
    /// [`SamplerError::NoStreamer`] when the session holds no disk streamer.
    #[must_use = "the id matches the event that lands the open"]
    pub fn open_stream(
        &mut self,
        path: impl Into<PathBuf>,
        place: Placement,
    ) -> Result<SamplerLoad, SamplerError> {
        let streamer = self
            .disk_streamer
            .as_ref()
            .ok_or(SamplerError::NoStreamer)?;
        let opening = streamer.open(&path.into(), place);
        let id = SamplerLoad::mint(&self.sampler_opens);
        self.jobs.submit(
            move || opening.wait(),
            move |_, done, events| {
                let event = match done {
                    Ok(Ok((stream, reader))) => SamplerEvent::Opened { id, stream, reader },
                    Ok(Err(error)) => SamplerEvent::OpenFailed {
                        id,
                        error: error.into(),
                    },
                    Err(failed) => SamplerEvent::OpenFailed {
                        id,
                        error: failed.into(),
                    },
                };
                events.push(event.into());
            },
        );
        Ok(id)
    }
}
