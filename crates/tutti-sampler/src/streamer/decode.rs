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
use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::Sender;
use tutti_buffers::Wave;
use tutti_platform::{StopToken, ThreadStart, start_thread};

use super::StreamerSender;
use super::cache::LruCache;
#[cfg(codec)]
use super::cache::wave_bytes;
use super::command::{Decoded, StreamId, StreamerCommand};
use super::handlers::Handles;
use super::metrics::{Metrics, ReadMeter};
use crate::OpenError;
use tutti_rt::RingFloat;

/// A hook a test runs where the streamer reads a file: the stream and the
/// frame it reads. It may block, or panic.
#[cfg(test)]
pub(super) type ReadHook = Arc<dyn Fn(StreamId, usize) + Send + Sync>;

/// A hook a test runs on the decode thread, before a file is decoded. It may
/// block, to hold a decode open.
#[cfg(test)]
pub(super) type DecodeHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// What a test can replace: how threads start, and what runs where the
/// streamer reads a file. Production is [`Seams::default`].
#[derive(Clone)]
pub(super) struct Seams {
    pub(super) spawn: ThreadStart,
    #[cfg(test)]
    pub(super) read_hook: Option<ReadHook>,
    #[cfg(test)]
    pub(super) decode_hook: Option<DecodeHook>,
}

impl Seams {
    /// These seams with threads started by `spawn`.
    pub(super) fn spawning(mut self, spawn: ThreadStart) -> Self {
        self.spawn = spawn;
        self
    }
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            spawn: start_thread,
            #[cfg(test)]
            read_hook: None,
            #[cfg(test)]
            decode_hook: None,
        }
    }
}

/// Why `file_path` could not be turned into audio: the file itself when it
/// cannot be opened, else a format this build cannot decode.
pub(super) fn unreadable(file_path: &Path) -> OpenError {
    match std::fs::File::open(file_path) {
        Err(error) => OpenError::Io(error.kind()),
        Ok(_) => OpenError::Format,
    }
}

/// A whole-file decode asked of the lane.
struct Job {
    id: StreamId,
    path: PathBuf,
}

/// The decode lane's queue, started on first use. Streamer-thread-local.
///
/// A decode runs on a thread of its own. When that thread cannot start, the
/// lane keeps its jobs and [`run_queued`](Self::run_queued) runs one per
/// streamer cycle.
pub(super) struct DecodeLane<F: RingFloat> {
    /// The streamer's own queue, where a result is sent.
    reports: StreamerSender<F>,
    /// The job queue, once the thread has started.
    jobs: Option<Sender<Job>>,
    /// The thread was refused: jobs wait here for the streamer's cycles.
    stepped: bool,
    queued: VecDeque<Job>,
    /// The read rate the stepped decodes publish.
    meter: ReadMeter,
}

impl<F: RingFloat> DecodeLane<F> {
    pub(super) fn new(reports: StreamerSender<F>) -> Self {
        Self {
            reports,
            jobs: None,
            stepped: false,
            queued: VecDeque::new(),
            meter: ReadMeter::new(),
        }
    }

    /// Whole-file decodes waiting for a cycle.
    pub(super) fn queued(&self) -> usize {
        self.queued.len()
    }

    /// Asks for `path` to be decoded whole for stream `id`, starting the thread
    /// if this is the first job. The answer arrives as a `Decoded` command.
    ///
    /// A thread that cannot start leaves the job queued for
    /// [`run_queued`](Self::run_queued).
    pub(super) fn submit(&mut self, shared: &Handles<F>, id: StreamId, path: PathBuf) {
        let job = Job { id, path };
        if self.stepped {
            self.queued.push_back(job);
            return;
        }
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return,
                // The thread is gone (a panic that escaped the job's own
                // containment): start another.
                Err(returned) => returned.0,
            },
            None => job,
        };
        match self.start(shared) {
            Ok(jobs) => {
                // The thread was just started with this queue: it is open.
                let _ = jobs.send(job);
                self.jobs = Some(jobs);
            }
            Err(error) => {
                tracing::debug!("decode: no thread ({error}); decoding on the streamer's cycles");
                self.stepped = true;
                self.queued.push_back(job);
            }
        }
    }

    /// Decodes the oldest queued job and sends its answer; whether one ran.
    /// A full command queue puts the job back (the decode is cached, so the
    /// next try answers from the cache).
    pub(super) fn run_queued(&mut self, shared: &Handles<F>) -> bool {
        let Some(job) = self.queued.pop_front() else {
            return false;
        };
        let cmd = decode_job(
            &shared.cache,
            &shared.metrics,
            &mut self.meter,
            #[cfg(test)]
            shared.seams.decode_hook.as_ref(),
            &job,
        );
        if self.reports.try_send(cmd).is_err() {
            self.queued.push_front(job);
            return false;
        }
        true
    }

    fn start(&self, shared: &Handles<F>) -> io::Result<Sender<Job>> {
        let (jobs, queue) = crossbeam_channel::unbounded::<Job>();
        let reports = self.reports.clone();
        let (cache, metrics) = (Arc::clone(&shared.cache), Arc::clone(&shared.metrics));
        #[cfg(test)]
        let decode_hook = shared.seams.decode_hook.clone();
        let body = move |_: &StopToken| {
            let mut meter = ReadMeter::new();
            for job in queue {
                let cmd = decode_job(
                    &cache,
                    &metrics,
                    &mut meter,
                    #[cfg(test)]
                    decode_hook.as_ref(),
                    &job,
                );
                if reports.send(cmd).is_err() {
                    break;
                }
            }
        };
        (shared.seams.spawn)("tutti-decode", None, Box::new(body))?.detach();
        Ok(jobs)
    }
}

/// Decodes `job` and answers it as the command the streamer applies. A panic
/// in the decoder is this file's failure, not the caller's.
fn decode_job<F: RingFloat>(
    cache: &LruCache<F>,
    metrics: &Metrics,
    meter: &mut ReadMeter,
    #[cfg(test)] decode_hook: Option<&DecodeHook>,
    job: &Job,
) -> StreamerCommand<F> {
    let wave = catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        if let Some(hook) = decode_hook {
            hook(&job.path);
        }
        load_wave(cache, metrics, meter, &job.path)
    }));
    let wave = match wave {
        Ok(Some(wave)) => Ok(wave),
        Ok(None) => Err(unreadable(&job.path)),
        Err(payload) => {
            tracing::error!(
                "decoding {} panicked: {}",
                job.path.display(),
                panic_message(payload.as_ref())
            );
            Err(OpenError::Format)
        }
    };
    StreamerCommand::Decoded {
        id: job.id,
        wave: Decoded(wave),
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
    file_path: &Path,
) -> Option<Arc<Wave<F>>> {
    if let Some(cached) = cache.get(file_path) {
        return Some(cached);
    }

    // `read_wave` is `tutti-codec`'s decode path, which is compiled only when a
    // codec feature is on — calling it unconditionally breaks the
    // `--no-default-features` build.
    #[cfg(codec)]
    {
        if let Ok(w) = tutti_codec::read_wave::<F>(file_path) {
            let arc_wave = Arc::new(w);
            meter.record_read(wave_bytes(&arc_wave), metrics);
            cache.insert(file_path.to_path_buf(), arc_wave.clone());
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
