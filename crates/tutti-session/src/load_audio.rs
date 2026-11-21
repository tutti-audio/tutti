//! A wave file as a value: [`Session::load_audio`] returns a [`LoadAudio`] that,
//! when the app awaits it, reads the file and resolves to a [`LoadedAudio`]
//! handle.
//!
//! A load holds the asset and the session's weak executor, and the read is a
//! task on that executor: awaiting it never blocks. Two loads of one asset are two values and two decodes; an app that
//! wants one copy keeps the handle and clones it. The session keeps no cache:
//! the wave lives while a clone of the handle does.

use std::fmt;
use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tutti_buffers::{Float, Wave};
use tutti_platform::{Asset, WeakExecutor};

use crate::loaded::{Stamp, caught};
use crate::unblocked::Unblocked;
use crate::{AppKey, Clock, LoadError, Session};

/// Where a file's samples are read from: [`LoadAudio::tier`]'s choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Tier {
    /// Decoded whole into memory.
    Resident,
    /// Read from disk by a stream. A file this build cannot stream fails the
    /// load with [`LoadError::NotStreamable`].
    Streamed,
    /// By the file's size: streamed when the file can be streamed and its
    /// decoded size (`frames * channels * size_of::<F>()` bytes in the
    /// session's float `F`) is more than `above` bytes, resident otherwise.
    ///
    /// A file whose header gives no length counts as past any threshold, so
    /// when it can be streamed it is.
    ///
    /// The choice is the file's alone: it does not ask whether the session has
    /// a disk streamer, so a streamed reading in a session built without one
    /// fails at [`open_stream`](Session::open_stream) with
    /// [`SamplerError::NoStreamer`](crate::SamplerError::NoStreamer), the call
    /// that needs the streamer.
    Auto {
        /// The decoded size, in bytes, past which the file streams.
        above: u64,
    },
}

impl Tier {
    /// The threshold of the default tier: 32 MiB of decoded samples, 95 seconds
    /// of 44.1 kHz stereo in `f32`.
    ///
    /// A resident wave costs its decoded size for as long as it lives; a stream
    /// costs a ring and a decode on the streamer's thread whatever the file's
    /// length. Samples, loops and stems of a minute or so sit under the line,
    /// where a wave in memory seeks and loops with no read behind it; a song or
    /// a long take sits over it.
    pub const STREAM_ABOVE_BYTES: u64 = 32 * 1024 * 1024;
}

impl Default for Tier {
    /// [`Auto`](Self::Auto) over [`STREAM_ABOVE_BYTES`](Self::STREAM_ABOVE_BYTES).
    fn default() -> Self {
        Self::Auto {
            above: Self::STREAM_ABOVE_BYTES,
        }
    }
}

/// A wave file to read, built by [`Session::load_audio`]. It is `Send`, and it
/// is a [`Future`] through [`IntoFuture`]: awaiting it reads the file as a task
/// on the session's executor and resolves to a [`LoadedAudio`]. There is no
/// blocking form.
///
/// It holds the asset and a weak handle to the session's executor, and nothing
/// else of the session. Dropping it unawaited reads nothing.
#[derive(Debug)]
#[must_use = "a load does nothing unless it is awaited"]
pub struct LoadAudio<F: Float = f32> {
    asset: Asset,
    executor: WeakExecutor,
    tier: Tier,
    float: PhantomData<fn() -> F>,
}

impl<F: Float> LoadAudio<F> {
    pub(crate) fn new(asset: Asset, executor: WeakExecutor) -> Self {
        Self {
            asset,
            executor,
            tier: Tier::default(),
            float: PhantomData,
        }
    }

    /// Where the samples are read from. The default is
    /// [`Tier::Auto`] over [`Tier::STREAM_ABOVE_BYTES`].
    pub const fn tier(mut self, tier: Tier) -> Self {
        self.tier = tier;
        self
    }

    /// Decodes the file whole into `F`, or reads its header when the tier
    /// streams it. The asset's [`Identity`](tutti_platform::Identity) is taken
    /// before the read, so a rewrite during it shows in [`LoadedAudio::changed`].
    fn run(self) -> Result<LoadedAudio<F>, LoadError> {
        let stamp = Stamp::take(self.asset.clone());
        let data = caught(|| self.read())?;
        Ok(LoadedAudio { stamp, data })
    }

    fn read(&self) -> Result<AudioData<F>, LoadError> {
        match self.tier {
            Tier::Resident => self.resident(),
            Tier::Streamed => {
                let facts = self.probe()?;
                if facts.streamable {
                    Ok(self.streamed(facts))
                } else {
                    Err(LoadError::NotStreamable(self.asset.name()))
                }
            }
            Tier::Auto { above } => {
                let facts = self.probe()?;
                // A decoded wave's size in bytes, saturating: a count past
                // `u64` is past any threshold.
                let wide = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
                let bytes = facts.frames.map_or(u64::MAX, |frames| {
                    wide(frames.get())
                        .saturating_mul(u64::from(facts.layout.count()))
                        .saturating_mul(wide(size_of::<F>()))
                });
                if facts.streamable && bytes > above {
                    Ok(self.streamed(facts))
                } else {
                    self.resident()
                }
            }
        }
    }

    fn probe(&self) -> Result<tutti_sampler::SampleFacts, LoadError> {
        tutti_sampler::probe(self.asset.clone()).map_err(|error| LoadError::Probe(Arc::new(error)))
    }

    fn streamed(&self, facts: tutti_sampler::SampleFacts) -> AudioData<F> {
        AudioData::Streamed(Arc::new(facts))
    }

    fn resident(&self) -> Result<AudioData<F>, LoadError> {
        tutti_codec::read_wave::<F>(self.asset.clone())
            .map(|wave| AudioData::Resident(Arc::new(wave)))
            .map_err(|error| LoadError::Decode(Arc::new(error)))
    }
}

impl<F: Float> IntoFuture for LoadAudio<F> {
    type Output = Result<LoadedAudio<F>, LoadError>;
    type IntoFuture = LoadingAudio<F>;

    fn into_future(self) -> LoadingAudio<F> {
        LoadingAudio {
            load: Unblocked::new(self.executor.clone(), self, Self::run),
        }
    }
}

/// A wave file load being awaited ([`LoadAudio`]): resolves to the
/// [`LoadedAudio`] once the file is read.
///
/// The first poll starts the read as a task on the session's executor (the
/// session's weak handle, upgraded for the spawn only) and awaits the receiver
/// of a oneshot. **The task fills the sender**: it takes the asset's
/// [`Identity`](tutti_platform::Identity), reads the file, and sends the handle
/// or the error, which wakes the future. A poll never blocks, and
/// `Session::update` is not needed.
///
/// Dropped before it resolves, the future drops the receiver: a read that has
/// not begun reads nothing, and one in flight finishes and its handle is dropped.
/// Dropped before its first poll it starts nothing.
///
/// # Errors
///
/// The output is the [`LoadError`] of a file that cannot be decoded, probed or
/// streamed; [`LoadError::Panicked`] when a decoder panicked;
/// [`LoadError::ShutDown`] when the session is gone.
///
/// # Panics
///
/// When polled after it resolved.
#[must_use = "a load does nothing unless it is awaited"]
pub struct LoadingAudio<F: Float = f32> {
    load: Unblocked<LoadAudio<F>, LoadedAudio<F>, LoadError>,
}

impl<F: Float> Future for LoadingAudio<F> {
    type Output = Result<LoadedAudio<F>, LoadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.load).poll(cx)
    }
}

impl<F: Float> fmt::Debug for LoadingAudio<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadingAudio").finish_non_exhaustive()
    }
}

/// What a [`LoadedAudio`] holds: the decoded wave, or the file's header.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AudioData<F: Float = f32> {
    /// The file decoded whole into the session's float. Every clone of the
    /// handle shares the `Arc`.
    Resident(Arc<Wave<F>>),
    /// The file's header; its samples stay on disk until a stream opens
    /// ([`Session::open_stream`] over [`LoadedAudio::asset`]). It is streamable,
    /// or the load would have failed with [`LoadError::NotStreamable`].
    Streamed(Arc<tutti_sampler::SampleFacts>),
}

/// A loaded wave file: its decoded wave (resident) or its header (streamed).
///
/// A clone shares what the load read. Releasing is dropping every clone: the
/// session holds none, so the wave is freed with the last one.
#[derive(Clone, Debug)]
pub struct LoadedAudio<F: Float = f32> {
    stamp: Stamp,
    data: AudioData<F>,
}

impl<F: Float> LoadedAudio<F> {
    /// The asset the load read.
    #[must_use]
    pub const fn asset(&self) -> &Asset {
        self.stamp.asset()
    }

    /// What the load read: the wave or the header.
    #[must_use]
    pub const fn data(&self) -> &AudioData<F> {
        &self.data
    }

    /// Whether the asset is not what was read: its [`Identity`](tutti_platform::Identity) (a file's path
    /// with its length and modification time) now differs from the one taken
    /// before the load. This reads the file's metadata, and nothing else.
    ///
    /// An asset of bytes never changes. A file stamped at the load reads
    /// changed when it is gone or differs; one that could not be stamped reads
    /// changed once it can be. To reload, await [`Session::load_audio`] over
    /// [`asset`](Self::asset) and swap the handle where it is used.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.stamp.changed()
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// A wave file to load, as a value the app awaits.
    ///
    /// It holds the session's weak executor and nothing else of the session, and
    /// the session keeps nothing of what it loads: two loads of one file are two
    /// values.
    pub fn load_audio(&self, asset: impl Into<Asset>) -> LoadAudio<C::Float> {
        LoadAudio::new(asset.into(), self.background.executor().downgrade())
    }
}
