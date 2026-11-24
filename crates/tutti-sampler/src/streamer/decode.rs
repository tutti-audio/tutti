//! Reading a file for an open: [`OpenStream`] is the work, [`OpenedStream`] what it found.
//!
//! A stream's ring is sized from the file's header and fed from a decoder that
//! seeks, or from the file decoded whole when it cannot seek (mp3, ogg: no frame
//! count, or a decoder that will not seek). Both are file reads that take as long
//! as the file is, so they run on the thread of the app that asked
//! ([`OpenStream::run`]), never on the streamer's, which only keeps rings fed.
//! The streamer takes the result back ([`DiskStreamer::adopt`](crate::DiskStreamer::adopt)).

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Wave};
use tutti_platform::Asset;
#[cfg(codec)]
use tutti_units::Samples;

use super::cache::LruCache;
#[cfg(codec)]
use super::cache::wave_bytes;
#[cfg(test)]
use super::command::StreamId;
#[cfg(codec)]
use super::metrics::Metrics;
#[cfg(codec)]
use super::metrics::ReadMeter;
use super::open::Opened;
use super::record::Record;
use crate::{OpenError, StreamStart};
use tutti_rt::RingFloat;

/// The frames one step of a whole-file decode reads.
#[cfg(codec)]
const DECODE_CHUNK: Samples = Samples::new(1 << 16);

/// A hook a test runs where the streamer reads a file: the stream and the
/// frame it reads. It may block, or panic.
#[cfg(test)]
pub(super) type ReadHook = Arc<dyn Fn(StreamId, usize) + Send + Sync>;

/// A hook a test runs where a whole-file decode starts, before a file is
/// decoded. It may panic.
#[cfg(test)]
pub(super) type DecodeHook = Arc<dyn Fn(&Asset) + Send + Sync>;

/// What a test can replace: what runs where the streamer reads a file and where an
/// open's decode starts. Production is [`Seams::default`].
#[derive(Clone, Default)]
pub(super) struct Seams {
    #[cfg(test)]
    pub(super) read_hook: Option<ReadHook>,
    #[cfg(test)]
    pub(super) decode_hook: Option<DecodeHook>,
}

/// Why `asset` could not be turned into audio: the source itself when it
/// cannot be opened, else a format this build cannot decode.
pub(super) fn unreadable(asset: &Asset) -> OpenError {
    match asset.open() {
        Err(error) => OpenError::Io(error.kind()),
        Ok(_) => OpenError::Format,
    }
}

/// What a stream's ring is fed from: a decoder that seeks in the file, or the
/// file decoded whole.
pub(crate) enum Source<F: RingFloat> {
    #[cfg(codec)]
    Stream(tutti_codec::WaveMetadata, Box<tutti_codec::FileIn<F>>),
    Whole(Arc<Wave<F>>),
}

/// An open the app runs: the reading of one file, for a stream that
/// [`DiskStreamer::adopt`](crate::DiskStreamer::adopt) then puts in service.
/// Made by [`DiskStreamer::open_stream`](crate::DiskStreamer::open_stream).
///
/// It holds nothing of the streamer but its wave cache, so it is `Send`: run
/// it on whichever thread the app reads files on.
#[must_use = "an open does nothing until it runs"]
pub struct OpenStream<F: RingFloat> {
    asset: Asset,
    start: StreamStart,
    width: Option<ChannelLayout>,
    cache: Arc<LruCache<F>>,
    #[cfg(codec)]
    metrics: Arc<Metrics>,
    #[cfg(test)]
    decode_hook: Option<DecodeHook>,
}

impl<F: RingFloat> std::fmt::Debug for OpenStream<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenStream")
            .field("asset", &self.asset.name())
            .field("start", &self.start)
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

impl<F: RingFloat> OpenStream<F> {
    pub(super) fn new(
        asset: Asset,
        start: StreamStart,
        width: Option<ChannelLayout>,
        cache: Arc<LruCache<F>>,
        #[cfg(codec)] metrics: Arc<Metrics>,
        #[cfg(test)] decode_hook: Option<DecodeHook>,
    ) -> Self {
        Self {
            asset,
            start,
            width,
            cache,
            #[cfg(codec)]
            metrics,
            #[cfg(test)]
            decode_hook,
        }
    }

    /// Reads the file: a seeking decoder when the file can seek (what
    /// [`probe`](crate::probe()) says), else the wave the cache already holds,
    /// else the file decoded whole, on this thread, which the cache then keeps.
    ///
    /// The streamer is not touched. A decoder that panics fails this open
    /// ([`OpenError::Format`]) and nothing else.
    ///
    /// # Errors
    ///
    /// [`OpenError::Io`] when the file cannot be opened, [`OpenError::Format`] when it is
    /// not audio this build decodes.
    pub fn run(self) -> Result<OpenedStream<F>, OpenError> {
        let source = match catch_unwind(AssertUnwindSafe(|| self.read())) {
            Ok(source) => source?,
            Err(payload) => {
                tracing::error!(
                    "opening {} panicked: {}",
                    self.asset.name(),
                    panic_message(payload.as_ref())
                );
                return Err(OpenError::Format);
            }
        };
        Ok(OpenedStream {
            asset: self.asset,
            start: self.start,
            width: self.width,
            stage: Stage::Read(source),
        })
    }

    /// The source of the stream: a seeking decoder, a cached wave, or the file
    /// decoded whole.
    ///
    /// The verdict on seeking comes from [`crate::probe()`], not from a local
    /// copy of the rule. A host picks a playback tier before any of this runs, and it
    /// must reach the same conclusion this does, so "can the streamer stream it" is asked
    /// in exactly one place.
    fn read(&self) -> Result<Source<F>, OpenError> {
        #[cfg(codec)]
        if let Some((meta, decoder)) = seeking_decoder(&self.asset) {
            return Ok(Source::Stream(meta, Box::new(decoder)));
        }
        let key = self.asset.identity();
        if let Some(wave) = key.as_ref().and_then(|key| self.cache.get(key)) {
            return Ok(Source::Whole(wave));
        }
        // With no decoder compiled in, a miss has nowhere to be filled.
        #[cfg(not(codec))]
        {
            Err(unreadable(&self.asset))
        }
        #[cfg(codec)]
        {
            #[cfg(test)]
            if let Some(hook) = &self.decode_hook {
                hook(&self.asset);
            }
            let mut meter = ReadMeter::new();
            let Ok(mut decode) = tutti_codec::WaveDecode::<F>::open(self.asset.clone()) else {
                return Err(unreadable(&self.asset));
            };
            loop {
                match decode.step(DECODE_CHUNK) {
                    Ok(Some(wave)) => {
                        let wave = Arc::new(wave);
                        meter.record_read(wave_bytes(&wave), &self.metrics);
                        // An asset with no identity is decoded each time and never cached.
                        if let Some(key) = key {
                            self.cache.insert(key, Arc::clone(&wave));
                        }
                        return Ok(Source::Whole(wave));
                    }
                    Ok(None) => {}
                    Err(_) => return Err(unreadable(&self.asset)),
                }
            }
        }
    }
}

/// An open's read file: what [`DiskStreamer::adopt`](crate::DiskStreamer::adopt) builds
/// the stream from. `Send`, so it crosses from the thread that read the file to the one
/// that owns the streamer.
#[must_use = "a read file is a stream only once adopted"]
pub struct OpenedStream<F: RingFloat> {
    pub(super) asset: Asset,
    pub(super) start: StreamStart,
    pub(super) width: Option<ChannelLayout>,
    pub(super) stage: Stage<F>,
}

/// How far an open has come: the file read, or the stream built from it and
/// waiting for room in the streamer's queue.
pub(super) enum Stage<F: RingFloat> {
    Read(Source<F>),
    Built(Box<Record<F>>, Opened<F>),
}

impl<F: RingFloat> std::fmt::Debug for OpenedStream<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedStream")
            .field("asset", &self.asset.name())
            .finish_non_exhaustive()
    }
}

/// Opens an incremental disk decoder for `asset`: `Some((metadata, decoder))` when the
/// format seeks (it has a frame count and the decoder opened), so the region can stream
/// real ranges from disk; `None` for the whole-file fallback, when the probe fails or the
/// format does not seek.
#[cfg(codec)]
fn seeking_decoder<F: RingFloat>(
    asset: &Asset,
) -> Option<(tutti_codec::WaveMetadata, tutti_codec::FileIn<F>)> {
    if !crate::probe(asset.clone()).ok()?.streamable {
        return None;
    }
    // Streamable, so both of these succeed, but they are still fallible calls and are
    // handled rather than unwrapped: the probe read the header a moment ago, and a file
    // can be replaced between the two reads.
    let meta = tutti_codec::read_wave_metadata(asset.clone()).ok()?;
    let decoder = tutti_codec::FileIn::<F>::open(asset.clone()).ok()?;
    Some((meta, decoder))
}

/// What a panic said, for the log.
pub(super) fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(not a string)")
}
