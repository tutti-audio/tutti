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
use tutti_sampler::{OpenError, Placement, Stream, StreamReader};

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
        place: Placement,
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

#[cfg(test)]
mod tests {
    use tutti_platform::Executor;
    use tutti_sampler::Placement;
    use tutti_units::SampleRate;

    use crate::{SamplerEvent, Session, SessionEvent};

    /// Updates a landing may take. A bound that only guards a hang: the streamer's task
    /// is woken by the bells' tick thread, which no count of updates orders against.
    const UPDATES: usize = 1_000_000;

    fn wav(path: &std::path::Path) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).expect("writes");
        for i in 0..8_000 {
            w.write_sample(i as f32 * 1e-5).expect("writes");
        }
        w.finalize().expect("writes");
    }

    /// **`update` gives a quiet streamer a cycle.** With a stream open and primed, and no
    /// command or ring pending, the streamer awaits its bell and runs no cycle; each
    /// `update` rings it, which is what sweeps a ring's retired controls that no voice
    /// rings for. Observed on the streamer's cycle count: it moves with updates alone.
    #[test]
    fn update_wakes_a_quiet_streamer_for_a_cycle() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("tone.wav");
        wav(&path);
        let mut s = Session::builder()
            .sample_rate(SampleRate::SR_8K)
            .executor(Executor::polled())
            .build()
            .expect("a session on a polled executor");
        let id = s.open_stream(&path, Placement::HEAD).expect("a streamer");
        let mut opened = false;
        for _ in 0..UPDATES {
            for event in s.update() {
                opened |= matches!(
                    event,
                    SessionEvent::Sampler(SamplerEvent::Opened { id: landed }) if landed == id
                );
            }
            if opened {
                break;
            }
            std::thread::yield_now();
        }
        assert!(opened, "the open never landed");
        // The streamer ran its whole poll before the job that carried the reply landed,
        // so it is awaiting its bell now.
        let before = s.sampler.streamer.as_ref().expect("a streamer").cycles();
        let mut woke = false;
        for _ in 0..UPDATES {
            let _ = s.update();
            if s.sampler.streamer.as_ref().expect("a streamer").cycles() > before {
                woke = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(woke, "updates alone never gave the streamer a cycle");
    }
}
