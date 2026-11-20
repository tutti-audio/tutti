//! A wave file as a value: [`Session::load_audio`] returns a [`LoadAudio`] that,
//! when the app runs it, reads the file and returns a [`LoadedAudio`] handle.
//!
//! A load holds the asset and nothing of the session, so the app runs it on any
//! thread. Two loads of one asset are two values and two decodes; an app that
//! wants one copy keeps the handle and clones it. The session keeps no cache:
//! the wave lives while a clone of the handle does.

use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tutti_buffers::{Float, Wave};
use tutti_platform::{Asset, Identity};

use crate::jobs::panic_message;
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
    /// decoded size (`frames * channels * size_of::<S>()` bytes in the
    /// session's float `S`) is more than `above` bytes, resident otherwise.
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

/// What a streamed [`LoadedAudio`]'s header says: the asset and what the
/// sampler can do with it.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    asset: Asset,
    facts: tutti_sampler::SampleFacts,
}

impl StreamInfo {
    /// The asset the load read.
    #[must_use]
    pub const fn asset(&self) -> &Asset {
        &self.asset
    }

    /// The file's header: length, rate and channels. The file is streamable, or
    /// the load would have failed with [`LoadError::NotStreamable`].
    #[must_use]
    pub const fn facts(&self) -> &tutti_sampler::SampleFacts {
        &self.facts
    }
}

/// A wave file to read, built by [`Session::load_audio`] and not yet run:
/// [`run`](Self::run) returns a [`LoadedAudio`].
///
/// It holds the asset and nothing of the session, so the app runs it on any
/// thread it chooses; dropping it unrun cancels it.
#[derive(Debug)]
#[must_use = "work does nothing until it runs"]
pub struct LoadAudio<S: Float = f32> {
    asset: Asset,
    tier: Tier,
    float: PhantomData<fn() -> S>,
}

impl<S: Float> LoadAudio<S> {
    pub(crate) fn new(asset: Asset) -> Self {
        Self {
            asset,
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

    /// Reads the file on the calling thread: decodes it whole into `S`, or
    /// reads its header when the tier streams it.
    ///
    /// The asset's [`Identity`] is taken before the read, so a rewrite during
    /// it shows in [`LoadedAudio::changed`].
    ///
    /// # Errors
    ///
    /// The [`LoadError`] of a file that cannot be decoded, probed or streamed;
    /// [`LoadError::Panicked`] when a decoder panicked.
    pub fn run(self) -> Result<LoadedAudio<S>, LoadError> {
        let loaded_from = self.asset.identity();
        let data = catch_unwind(AssertUnwindSafe(|| self.read())).unwrap_or_else(|payload| {
            Err(LoadError::Panicked {
                message: panic_message(payload.as_ref()),
            })
        })?;
        Ok(LoadedAudio {
            asset: self.asset,
            loaded_from,
            data,
        })
    }

    fn read(&self) -> Result<AudioData<S>, LoadError> {
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
                let bytes = facts.frames.map_or(0, |frames| {
                    wide(frames.get())
                        .saturating_mul(u64::from(facts.layout.count()))
                        .saturating_mul(wide(size_of::<S>()))
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

    fn streamed(&self, facts: tutti_sampler::SampleFacts) -> AudioData<S> {
        AudioData::Streamed(Arc::new(StreamInfo {
            asset: self.asset.clone(),
            facts,
        }))
    }

    fn resident(&self) -> Result<AudioData<S>, LoadError> {
        tutti_codec::read_wave::<S>(self.asset.clone())
            .map(|wave| AudioData::Resident(Arc::new(wave)))
            .map_err(|error| LoadError::Decode(Arc::new(error)))
    }
}

/// What a [`LoadedAudio`] holds: the decoded wave, or the file's header.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AudioData<S: Float = f32> {
    /// The file decoded whole into the session's float. Every clone of the
    /// handle shares the `Arc`.
    Resident(Arc<Wave<S>>),
    /// The file's header; its samples stay on disk until a stream opens
    /// ([`Session::open_stream`] over [`StreamInfo::asset`]).
    Streamed(Arc<StreamInfo>),
}

/// A loaded wave file: its decoded wave (resident) or its header (streamed).
///
/// A clone shares what the load read. Releasing is dropping every clone: the
/// session holds none, so the wave is freed with the last one.
#[derive(Clone, Debug)]
pub struct LoadedAudio<S: Float = f32> {
    asset: Asset,
    loaded_from: Option<Identity>,
    data: AudioData<S>,
}

impl<S: Float> LoadedAudio<S> {
    /// The asset the load read.
    #[must_use]
    pub const fn asset(&self) -> &Asset {
        &self.asset
    }

    /// What the load read: the wave or the header.
    #[must_use]
    pub const fn data(&self) -> &AudioData<S> {
        &self.data
    }

    /// Whether the asset is not what was read: its [`Identity`] (a file's path
    /// with its length and modification time) now differs from the one taken
    /// before the load. This reads the file's metadata, and nothing else.
    ///
    /// An asset of bytes never changes. A file stamped at the load reads
    /// changed when it is gone or differs; one that could not be stamped reads
    /// changed once it can be. To reload, run
    /// [`Session::load_audio`] over [`asset`](Self::asset) and swap the handle
    /// where it is used.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.asset.identity() != self.loaded_from
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// A wave file to load, as a value the app runs where it likes.
    ///
    /// It holds nothing of the session, and the session keeps nothing of what
    /// it loads: two loads of one file are two values.
    pub fn load_audio(&self, asset: impl Into<Asset>) -> LoadAudio<C::Float> {
        LoadAudio::new(asset.into())
    }
}
