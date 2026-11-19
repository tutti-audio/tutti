//! The decode lane: whole-file decodes off the streamer's thread.
//!
//! A file that cannot seek (mp3, ogg: no frame count, or a decoder that will
//! not seek) is decoded whole before its stream can exist, and that takes as
//! long as the file is. Done inside the command drain it stopped every other
//! stream's refill for its duration and back-pressured the caller's bounded
//! queue. It is one thread here, started on first use and fed one job at a
//! time (a burst of opens costs one core, never the streamer's), and its
//! result comes back through the streamer's own command queue as
//! [`Decoded`](super::command::StreamerCommand::Decoded), so the streamer
//! wakes for it as it wakes for any command.
//!
//! The thread is detached, not joined: it ends after the job it is on when the
//! lane is dropped (the job queue disconnects), or when the streamer's queue is
//! gone (its send fails).

// With no codec feature there is no decode to run: the lane is never asked for
// a job, and `load_wave` reads only the cache. A feature-matrix artifact, so
// scoped to that build (as `metrics.rs` and `cache.rs` are).
#![cfg_attr(
    not(codec),
    allow(
        dead_code,
        reason = "the decode lane's only caller is the codec-gated whole-file arm of `begin`"
    )
)]

use std::any::Any;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crossbeam_channel::Sender;
use tutti_buffers::Wave;
use tutti_platform::{Asset, PriorityRequest, StopToken, ThreadHandle, spawn_named};

use super::StreamerSender;
use super::cache::LruCache;
#[cfg(codec)]
use super::cache::wave_bytes;
use super::command::{Decoded, StreamId, StreamerCommand};
use super::handlers::Handles;
use super::metrics::{Metrics, ReadMeter};
use crate::OpenError;
use tutti_rt::RingFloat;

/// How a thread is started: the seam a test replaces to see a spawn fail.
pub(super) type Spawner = fn(
    &str,
    Option<PriorityRequest>,
    Box<dyn FnOnce(&StopToken) + Send>,
) -> io::Result<ThreadHandle<()>>;

/// The OS's thread: what ships.
fn os_spawn(
    name: &str,
    priority: Option<PriorityRequest>,
    body: Box<dyn FnOnce(&StopToken) + Send>,
) -> io::Result<ThreadHandle<()>> {
    spawn_named(name, priority, body)
}

/// A hook a test runs where the streamer reads a file: the stream and the
/// frame it reads. It may block, or panic.
#[cfg(test)]
pub(super) type ReadHook = Arc<dyn Fn(StreamId, usize) + Send + Sync>;

/// A hook a test runs on the decode thread, before a file is decoded. It may
/// block, to hold a decode open.
#[cfg(test)]
pub(super) type DecodeHook = Arc<dyn Fn(&Asset) + Send + Sync>;

/// What a test can replace: how threads start, and what runs where the
/// streamer reads a file. Production is [`Seams::default`].
#[derive(Clone)]
pub(super) struct Seams {
    pub(super) spawn: Spawner,
    #[cfg(test)]
    pub(super) read_hook: Option<ReadHook>,
    #[cfg(test)]
    pub(super) decode_hook: Option<DecodeHook>,
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            spawn: os_spawn,
            #[cfg(test)]
            read_hook: None,
            #[cfg(test)]
            decode_hook: None,
        }
    }
}

/// Why `asset` could not be turned into audio: the source itself when it
/// cannot be opened, else a format this build cannot decode.
pub(super) fn unreadable(asset: &Asset) -> OpenError {
    match asset.open() {
        Err(error) => OpenError::Io(error.kind()),
        Ok(_) => OpenError::Format,
    }
}

/// A whole-file decode asked of the lane.
struct Job {
    id: StreamId,
    asset: Asset,
}

/// The decode thread's queue, started on first use. Streamer-thread-local.
pub(super) struct DecodeLane<F: RingFloat> {
    /// The streamer's own queue, where a result is sent.
    reports: StreamerSender<F>,
    /// The job queue, once the thread has started.
    jobs: Option<Sender<Job>>,
}

impl<F: RingFloat> DecodeLane<F> {
    pub(super) fn new(reports: StreamerSender<F>) -> Self {
        Self {
            reports,
            jobs: None,
        }
    }

    /// Asks for `asset` to be decoded whole for stream `id`, starting the thread
    /// if this is the first job. The answer arrives as a `Decoded` command.
    ///
    /// # Errors
    ///
    /// [`OpenError::Io`] when the thread cannot be started, so the open fails
    /// where it would have waited for good.
    pub(super) fn submit(
        &mut self,
        shared: &Handles<F>,
        id: StreamId,
        asset: Asset,
    ) -> Result<(), OpenError> {
        let job = Job { id, asset };
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return Ok(()),
                // The thread is gone (a panic that escaped the job's own
                // containment): start another.
                Err(returned) => returned.0,
            },
            None => job,
        };
        let jobs = self.start(shared)?;
        let sent = jobs.send(job);
        self.jobs = Some(jobs);
        sent.map_err(|_| OpenError::Io(io::ErrorKind::BrokenPipe))
    }

    fn start(&self, shared: &Handles<F>) -> Result<Sender<Job>, OpenError> {
        let (jobs, queue) = crossbeam_channel::unbounded::<Job>();
        let reports = self.reports.clone();
        let (cache, metrics) = (Arc::clone(&shared.cache), Arc::clone(&shared.metrics));
        #[cfg(test)]
        let decode_hook = shared.seams.decode_hook.clone();
        let body = move |_: &StopToken| {
            let mut meter = ReadMeter::new();
            for Job { id, asset } in queue {
                // A panic in the decoder is this file's failure, not the
                // thread's: the next job runs.
                let wave = catch_unwind(AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if let Some(hook) = &decode_hook {
                        hook(&asset);
                    }
                    load_wave(&cache, &metrics, &mut meter, &asset)
                }));
                let wave = match wave {
                    Ok(Some(wave)) => Ok(wave),
                    Ok(None) => Err(unreadable(&asset)),
                    Err(payload) => {
                        tracing::error!(
                            "decoding {} panicked: {}",
                            asset.name(),
                            panic_message(payload.as_ref())
                        );
                        Err(OpenError::Format)
                    }
                };
                let cmd = StreamerCommand::Decoded {
                    id,
                    wave: Decoded(wave),
                };
                if reports.send(cmd).is_err() {
                    break;
                }
            }
        };
        (shared.seams.spawn)("tutti-decode", None, Box::new(body))
            .map_err(|error| OpenError::Io(error.kind()))?
            .detach();
        Ok(jobs)
    }
}

/// The whole-file fallback: hand back a cached [`Wave`], or decode one and cache
/// it. `None` means the file could not be turned into audio.
///
/// With no codec feature enabled there is no decoder to call, so the miss arm is
/// gated out and a miss is simply `None`. The cache *lookup* stays live in both
/// builds: entries reach it from elsewhere, and a hit needs no decoder.
fn load_wave<F: RingFloat>(
    cache: &LruCache<F>,
    metrics: &Metrics,
    meter: &mut ReadMeter,
    asset: &Asset,
) -> Option<Arc<Wave<F>>> {
    // Taken once: a hit and the insert after a miss use the same key. An asset
    // with no identity is decoded each time and never cached.
    let key = asset.identity();
    if let Some(cached) = key.as_ref().and_then(|key| cache.get(key)) {
        return Some(cached);
    }

    // `read_wave` is `tutti-codec`'s decode path, which is compiled only when a
    // codec feature is on — calling it unconditionally breaks the
    // `--no-default-features` build.
    #[cfg(codec)]
    {
        if let Ok(w) = tutti_codec::read_wave::<F>(asset.clone()) {
            let arc_wave = Arc::new(w);
            meter.record_read(wave_bytes(&arc_wave), metrics);
            if let Some(key) = key {
                cache.insert(key, arc_wave.clone());
            }
            return Some(arc_wave);
        }
    }

    // Silences the unused-parameter warning in the codec-free build, where the
    // only way to reach this line is a cache miss with nothing able to fill it.
    #[cfg(not(codec))]
    let _ = (metrics, meter);

    None
}

/// What a panic said, for the log.
pub(super) fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(not a string)")
}
