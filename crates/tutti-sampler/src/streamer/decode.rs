//! The decode lane: whole-file decodes off the streamer's cycles.
//!
//! A file that cannot seek (mp3, ogg: no frame count, or a decoder that will
//! not seek) is decoded whole before its stream can exist, and that takes as
//! long as the file is. Done inside the command drain it stopped every other
//! stream's refill for its duration and back-pressured the caller's bounded
//! queue. Each decode is a future that yields between
//! chunks of [`DECODE_CHUNK`] frames, so one poll stays short; its result comes
//! back through the streamer's own command queue as
//! [`Decoded`](super::command::StreamerCommand::Decoded), so the streamer wakes for
//! it as it wakes for any command.
//!
//! A decode ends after the answer it sends, or when the streamer's queue is gone
//! (its send fails). The lane holds the decodes and the streamer's cycles poll them,
//! so no executor is held by the streamer's future.

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
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Waker};
#[cfg(codec)]
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use crossbeam_channel::TrySendError;
use tutti_buffers::Wave;
use tutti_platform::Asset;
use tutti_units::Samples;

use super::StreamerSender;
use super::cache::LruCache;
#[cfg(codec)]
use super::cache::wave_bytes;
use super::command::{Decoded, StreamId, StreamerCommand};
use super::handlers::Handles;
use super::loop_body::yield_now;
use super::metrics::{Metrics, ReadMeter};
use crate::OpenError;
use tutti_rt::RingFloat;

/// The frames one poll of a decode reads before it yields.
const DECODE_CHUNK: Samples = Samples::new(1 << 16);

/// A hook a test runs where the streamer reads a file: the stream and the
/// frame it reads. It may block, or panic.
#[cfg(test)]
pub(super) type ReadHook = Arc<dyn Fn(StreamId, usize) + Send + Sync>;

/// A hook a test runs where a decode starts, before a file is decoded. It may
/// block, to hold a decode open.
#[cfg(test)]
pub(super) type DecodeHook = Arc<dyn Fn(&Asset) + Send + Sync>;

/// What a test can replace: what runs where the streamer reads a file and where a
/// decode starts. Production is [`Seams::default`].
#[derive(Clone, Default)]
pub(super) struct Seams {
    #[cfg(test)]
    pub(super) read_hook: Option<ReadHook>,
    #[cfg(test)]
    pub(super) decode_hook: Option<DecodeHook>,
    /// The frames a decode reads per poll, in place of [`DECODE_CHUNK`].
    #[cfg(test)]
    pub(super) decode_chunk: Option<Samples>,
}

/// Why `asset` could not be turned into audio: the source itself when it
/// cannot be opened, else a format this build cannot decode.
pub(super) fn unreadable(asset: &Asset) -> OpenError {
    match asset.open() {
        Err(error) => OpenError::Io(error.kind()),
        Ok(_) => OpenError::Format,
    }
}

/// A decode as the lane holds it.
type Decode = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The decode lane: the decodes in flight, polled by the streamer's own cycles, one
/// chunk each. Streamer-local, so the streamer's future holds no executor.
pub(super) struct DecodeLane<F: RingFloat> {
    /// The streamer's own queue, where a result is sent.
    reports: StreamerSender<F>,
    /// In a `Mutex` only for `Sync` (a streamer is held by shared reference); the lane
    /// is streamer-local and only ever reaches it through `get_mut`.
    active: Mutex<Vec<Decode>>,
}

impl<F: RingFloat> DecodeLane<F> {
    pub(super) fn new(reports: StreamerSender<F>) -> Self {
        Self {
            reports,
            active: Mutex::new(Vec::new()),
        }
    }

    /// Starts decoding `asset` whole for stream `id`. The answer arrives as a
    /// `Decoded` command, after enough [`poll`](Self::poll)s.
    pub(super) fn submit(&mut self, shared: &Handles<F>, id: StreamId, asset: Asset) {
        let reports = self.reports.clone();
        let (cache, metrics) = (Arc::clone(&shared.cache), Arc::clone(&shared.metrics));
        #[cfg(test)]
        let decode_hook = shared.seams.decode_hook.clone();
        #[cfg(test)]
        let chunk = shared.seams.decode_chunk.unwrap_or(DECODE_CHUNK);
        #[cfg(not(test))]
        let chunk = DECODE_CHUNK;
        self.active_mut().push(Box::pin(async move {
            #[cfg(test)]
            if let Some(hook) = &decode_hook {
                hook(&asset);
            }
            let wave = decode_whole(&cache, &metrics, &asset, chunk).await;
            send_decoded(&reports, id, wave).await;
        }));
    }

    /// Polls every decode once, which reads one chunk of each; whether any is still
    /// running. The streamer polls again after yielding, so the waker is not used.
    pub(super) fn poll(&mut self) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        let active = self.active_mut();
        active.retain_mut(|decode| decode.as_mut().poll(&mut cx).is_pending());
        !active.is_empty()
    }

    fn active_mut(&mut self) -> &mut Vec<Decode> {
        self.active
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The wave of `asset` or why there is none. A panic in the decoder is this file's
/// failure, not the executor's.
async fn decode_whole<F: RingFloat>(
    cache: &LruCache<F>,
    metrics: &Metrics,
    asset: &Asset,
    chunk: Samples,
) -> Result<Arc<Wave<F>>, OpenError> {
    match load_wave(cache, metrics, asset, chunk).await {
        Ok(Some(wave)) => Ok(wave),
        Ok(None) => Err(unreadable(asset)),
        Err(payload) => {
            tracing::error!(
                "decoding {} panicked: {}",
                asset.name(),
                panic_message(payload.as_ref())
            );
            Err(OpenError::Format)
        }
    }
}

/// Sends the answer without blocking: a full queue yields and tries again, a
/// gone streamer drops it.
async fn send_decoded<F: RingFloat>(
    reports: &StreamerSender<F>,
    id: StreamId,
    wave: Result<Arc<Wave<F>>, OpenError>,
) {
    let mut cmd = StreamerCommand::Decoded {
        id,
        wave: Decoded(wave),
    };
    loop {
        match reports.try_send(cmd) {
            Err(TrySendError::Full(back)) => {
                cmd = back;
                yield_now().await;
            }
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
        }
    }
}

/// The whole-file fallback: hand back a cached [`Wave`], or decode one and cache
/// it. `None` means the file could not be turned into audio.
///
/// With no codec feature enabled there is no decoder to call, so the miss arm is
/// gated out and a miss is simply `None`. The cache *lookup* stays live in both
/// builds: entries reach it from elsewhere, and a hit needs no decoder.
async fn load_wave<F: RingFloat>(
    cache: &LruCache<F>,
    metrics: &Metrics,
    asset: &Asset,
    chunk: Samples,
) -> Result<Option<Arc<Wave<F>>>, Box<dyn Any + Send>> {
    // Taken once: a hit and the insert after a miss use the same key. An asset
    // with no identity is decoded each time and never cached.
    let key = asset.identity();
    if let Some(cached) = key.as_ref().and_then(|key| cache.get(key)) {
        return Ok(Some(cached));
    }

    // `WaveDecode` is `tutti-codec`'s decode path, which is compiled only when a
    // codec feature is on — calling it unconditionally breaks the
    // `--no-default-features` build.
    #[cfg(codec)]
    {
        let mut meter = ReadMeter::new();
        let Ok(mut decode) = tutti_codec::WaveDecode::<F>::open(asset.clone()) else {
            return Ok(None);
        };
        loop {
            match catch_unwind(AssertUnwindSafe(|| decode.step(chunk)))? {
                Ok(Some(wave)) => {
                    let arc_wave = Arc::new(wave);
                    meter.record_read(wave_bytes(&arc_wave), metrics);
                    if let Some(key) = key {
                        cache.insert(key, arc_wave.clone());
                    }
                    return Ok(Some(arc_wave));
                }
                Ok(None) => yield_now().await,
                Err(_) => return Ok(None),
            }
        }
    }

    // Silences the unused-parameter warning in the codec-free build, where the
    // only way to reach this line is a cache miss with nothing able to fill it.
    #[cfg(not(codec))]
    {
        let _ = (metrics, ReadMeter::new(), chunk);
        Ok(None)
    }
}

/// What a panic said, for the log.
pub(super) fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(not a string)")
}
