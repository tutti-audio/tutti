//! The session's sampler: a disk stream opened on the session's executor, and
//! what lands from it in [`Session::update`](crate::Session::update).
//!
//! The streamer opens a file on a thread of its own, so an open takes as long
//! as the file's header and its first reads. [`Session::open_stream`] starts
//! one and returns at once; the open ends as
//! [`SamplerEvent::Opened`] with the call's [`SamplerLoad`], under
//! [`SessionEvent::Sampler`], and the stream is then
//! [taken](Session::take_stream) by that id in the session's float; or it ends
//! as [`SamplerEvent::OpenFailed`] with the reason.
//!
//! What a file is, resident or streamed, is the pool's: an
//! `audio_file` entry (with a `tutti-codec` decoder on) reads as an
//! `AudioEntry`, and a streamed one names the path
//! [`open_stream`](Session::open_stream) opens.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tutti_platform::Asset;
use tutti_sampler::{OpenError, Stream, StreamReader, StreamStart};

use tutti_rt::LockExt;

use crate::WorkerError;
use crate::{Session, SessionEvent};

tutti_rt::id! {
    /// An open ([`Session::open_stream`]) the session started, named so the event that lands it can be matched to the call:
    /// [`SamplerEvent::Opened`] and [`SamplerEvent::OpenFailed`] carry it. The
    /// session mints it; it names nothing else.
    pub struct SamplerLoad;
}

/// The opened streams a session holds until the host takes them: both halves
/// of each, in the session's float, by the id of the open that made them.
pub(crate) type OpenedStreams<F> = Arc<Mutex<HashMap<SamplerLoad, (Stream<F>, StreamReader<F>)>>>;

/// A sampler event of the session, one per open that ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum SamplerEvent {
    /// An open [`Session::open_stream`] started finished: the stream is
    /// running and waits in the session for the host to
    /// [`take`](Session::take_stream) it by `id`.
    Opened {
        /// The id [`Session::open_stream`] returned.
        id: SamplerLoad,
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
    /// The open's job did not finish: it panicked, or the executor dropped it.
    #[error(transparent)]
    Job(#[from] WorkerError),
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// Opens `path` as a disk stream, its reader starting at `place`, on the
    /// session's executor. Returns at once; the stream lands as
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
        asset: impl Into<Asset>,
        place: StreamStart,
    ) -> Result<SamplerLoad, SamplerError> {
        let streamer = self
            .sampler
            .streamer
            .as_ref()
            .ok_or(SamplerError::NoStreamer)?;
        let opening = streamer.open(asset, place);
        let id = SamplerLoad::mint(&self.sampler.opens);
        let landed = Arc::clone(&self.sampler.streams);
        self.jobs.submit_future(opening, move |_, done, events| {
            let event = match done {
                Ok(Ok(opened)) => {
                    landed.lock_recover().insert(id, opened);
                    SamplerEvent::Opened { id }
                }
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
        });
        Ok(id)
    }

    /// The stream an open landed ([`SamplerEvent::Opened`]): the voice's
    /// reader and the control side, in the session's float. The reader is the
    /// voice's (`DiskVoice::from_reader`); the stream is the control side,
    /// which a `VoiceNode::streaming` or a pool's `add_streaming` takes, so the
    /// voice sounds for as long as the node or the pool slot lives. Dropping the
    /// stream ends it.
    ///
    /// `None` for an id that has not landed, failed, or was taken already. A
    /// stream no one takes is dropped with the session.
    #[must_use = "the stream ends when it is dropped"]
    pub fn take_stream(
        &mut self,
        id: SamplerLoad,
    ) -> Option<(Stream<C::Float>, StreamReader<C::Float>)> {
        self.sampler.streams.lock_recover().remove(&id)
    }
}
