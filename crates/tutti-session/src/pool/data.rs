//! The data entries: a file or a SoundFont read whole, loaded once, shared by
//! its `Arc`.

use std::sync::Arc;

use tutti_platform::Asset;

use crate::pool::entry::{Declared, Life, Load, LoadError, PoolEntry, Sharing};

/// A wave file: [`audio_file`]'s entry, decoded whole (resident) or probed for
/// a disk stream ([`streamed`](Declared::streamed)).
///
/// Its identity is the asset's (see [`audio_file`]) and the tier ([`Tier`]).
#[cfg(all(feature = "sampler", codec))]
#[derive(Clone, Debug)]
pub struct AudioFile {
    asset: Asset,
    tier: Tier,
}

/// Where a file's samples are read from: half of [`AudioFile`]'s identity.
#[cfg(all(feature = "sampler", codec))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Tier {
    /// Decoded whole into memory.
    Resident,
    /// Read from disk by a stream.
    Streamed,
    /// By the file's size: streamed when the file can be streamed and its
    /// decoded size (`frames * channels * size_of::<S>()` bytes in the session's
    /// float `S`) is more than
    /// `above` bytes, resident otherwise.
    Auto {
        /// The decoded size, in bytes, past which the file streams.
        above: u64,
    },
}

/// What a landed [`audio_file`] entry reads as: the decoded wave of a resident
/// entry, or what a streamed entry's header says.
///
/// A handle is a clone and owns nothing: dropping it leaves the entry loaded.
#[cfg(all(feature = "sampler", codec))]
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AudioEntry<S: tutti_buffers::Float = f32> {
    /// The file, decoded whole into the session's float; every handle shares
    /// the `Arc`.
    Resident(Arc<tutti_buffers::Wave<S>>),
    /// The file's header, read; its samples stay on disk until a stream opens
    /// ([`Session::open_stream`](crate::Session::open_stream) over
    /// [`StreamInfo::asset`]).
    Streamed(Arc<StreamInfo>),
}

/// What a streamed [`audio_file`] entry's header says: the asset and what the
/// sampler can do with it.
#[cfg(all(feature = "sampler", codec))]
#[derive(Clone, Debug)]
pub struct StreamInfo {
    asset: Asset,
    facts: tutti_sampler::SampleFacts,
}

#[cfg(all(feature = "sampler", codec))]
impl StreamInfo {
    /// The asset the entry declared.
    #[must_use]
    pub const fn asset(&self) -> &Asset {
        &self.asset
    }

    /// The file's header: length, rate and channels. The file is streamable, or
    /// the entry would have failed with [`LoadError::NotStreamable`].
    #[must_use]
    pub const fn facts(&self) -> &tutti_sampler::SampleFacts {
        &self.facts
    }
}

#[cfg(all(feature = "sampler", codec))]
impl AudioFile {
    /// The default threshold of [`auto`](Declared::auto): 32 MiB of decoded
    /// samples, 95 seconds of 44.1 kHz stereo in `f32`.
    ///
    /// A resident wave costs its decoded size for as long as it lives, and a
    /// session holds one per file in use; a stream costs a ring and a decode
    /// on the streamer's thread whatever the file's length. Samples, loops and
    /// stems of a minute or so sit under the line, where a wave in memory seeks
    /// and loops with no read behind it; a song or a long take sits over it.
    pub const STREAM_ABOVE_BYTES: u64 = 32 * 1024 * 1024;
}

/// Declares the wave `asset` (a path, or bytes) under `key`, decoded whole; the
/// entry reads as [`AudioEntry::Resident`]. Another key declared with the same
/// asset shares the one decode, and every handle is the same `Arc`.
///
/// Two declarations are of the same asset when their [`Asset`]s are equal (the
/// same path, or clones of one bytes asset): the diff reads no file, so a file
/// rewritten since loads again at [`refresh_pool`](crate::Session::refresh_pool).
#[cfg(all(feature = "sampler", codec))]
pub fn audio_file<K>(key: K, asset: impl Into<Asset>) -> Declared<K, AudioFile, Sharing> {
    Declared::shared(
        key,
        AudioFile {
            asset: asset.into(),
            tier: Tier::Resident,
        },
    )
}

#[cfg(all(feature = "sampler", codec))]
impl<K, M> Declared<K, AudioFile, M> {
    /// The file played from disk: its entry reads as [`AudioEntry::Streamed`]
    /// and fails with [`LoadError::NotStreamable`] for a file this build cannot
    /// stream. The tier is part of the identity: changing it reloads.
    pub fn streamed(mut self) -> Self {
        self.entry.tier = Tier::Streamed;
        self
    }

    /// The tier chosen by the file's size, past [`AudioFile::STREAM_ABOVE_BYTES`]:
    /// [`streamed_above`](Self::streamed_above) with that threshold.
    pub fn auto(self) -> Self {
        self.streamed_above(AudioFile::STREAM_ABOVE_BYTES)
    }

    /// The tier chosen by the file's size: the entry reads as
    /// [`AudioEntry::Streamed`] when the file can be streamed and its decoded
    /// size (`frames * channels * 4` bytes) is more than `bytes`, and as
    /// [`AudioEntry::Resident`] otherwise, a file that cannot be streamed
    /// included. The threshold is part of the identity: changing it reloads.
    ///
    /// The choice is the file's alone: it does not ask whether the session has
    /// a disk streamer, so a streamed reading of a session built without one
    /// fails at [`open_stream`](crate::Session::open_stream) with
    /// [`SamplerError::NoStreamer`](crate::SamplerError::NoStreamer), the call
    /// that needs the streamer.
    pub fn streamed_above(mut self, bytes: u64) -> Self {
        self.entry.tier = Tier::Auto { above: bytes };
        self
    }
}

#[cfg(all(feature = "sampler", codec))]
impl<S: tutti_buffers::Float> PoolEntry<S> for AudioFile {
    type Identity = (Asset, Tier);
    type Loaded = AudioEntry<S>;
    type Handle = AudioEntry<S>;

    fn identity(&self) -> Self::Identity {
        (self.asset.clone(), self.tier)
    }

    fn load(&self) -> Load<Self::Loaded, S> {
        let (asset, tier) = (self.asset.clone(), self.tier);
        Load::on_worker(move || match tier {
            Tier::Resident => resident::<S>(asset),
            Tier::Streamed => {
                let facts = probe(&asset)?;
                if facts.streamable {
                    Ok(streamed(asset, facts))
                } else {
                    Err(LoadError::NotStreamable(asset.name()))
                }
            }
            Tier::Auto { above } => {
                let facts = probe(&asset)?;
                // A decoded wave's size in bytes, saturating: a count past
                // `u64` is past any threshold.
                let wide = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
                let bytes = facts.frames.map_or(0, |frames| {
                    wide(frames.get())
                        .saturating_mul(u64::from(facts.layout.count()))
                        .saturating_mul(wide(size_of::<S>()))
                });
                if facts.streamable && bytes > above {
                    Ok(streamed(asset, facts))
                } else {
                    resident::<S>(asset)
                }
            }
        })
    }

    fn asset(&self) -> Option<Asset> {
        Some(self.asset.clone())
    }

    fn handle(loaded: &Self::Loaded, _: &Life) -> Self::Handle {
        loaded.clone()
    }
}

#[cfg(all(feature = "sampler", codec))]
fn probe(asset: &Asset) -> Result<tutti_sampler::SampleFacts, LoadError> {
    tutti_sampler::probe(asset.clone()).map_err(|error| LoadError::Probe(Arc::new(error)))
}

#[cfg(all(feature = "sampler", codec))]
fn streamed<S: tutti_buffers::Float>(
    asset: Asset,
    facts: tutti_sampler::SampleFacts,
) -> AudioEntry<S> {
    AudioEntry::Streamed(Arc::new(StreamInfo { asset, facts }))
}

#[cfg(all(feature = "sampler", codec))]
fn resident<S: tutti_buffers::Float>(asset: Asset) -> Result<AudioEntry<S>, LoadError> {
    tutti_codec::read_wave::<S>(asset)
        .map(|wave| AudioEntry::Resident(Arc::new(wave)))
        .map_err(|error| LoadError::Decode(Arc::new(error)))
}

/// A SoundFont, parsed: [`soundfont`]'s entry.
///
/// Its identity is the asset's (see [`soundfont`]).
#[cfg(feature = "soundfont")]
#[derive(Clone, Debug)]
pub struct SoundFontFile {
    asset: Asset,
}

/// Declares the SoundFont `asset` (a path, or bytes) under `key`. Another key
/// declared with the same asset shares the one parse, and every handle is the
/// same `Arc`. Two declarations are of the same asset as [`audio_file`]'s are.
#[cfg(feature = "soundfont")]
pub fn soundfont<K>(key: K, asset: impl Into<Asset>) -> Declared<K, SoundFontFile, Sharing> {
    Declared::shared(
        key,
        SoundFontFile {
            asset: asset.into(),
        },
    )
}

#[cfg(feature = "soundfont")]
impl<S: tutti_buffers::Float> PoolEntry<S> for SoundFontFile {
    type Identity = Asset;
    type Loaded = Arc<tutti_sf2::SoundFont>;
    type Handle = Arc<tutti_sf2::SoundFont>;

    fn identity(&self) -> Asset {
        self.asset.clone()
    }

    fn load(&self) -> Load<Self::Loaded, S> {
        let asset = self.asset.clone();
        Load::on_worker(move || {
            tutti_sf2::SoundFont::open(asset)
                .map(Arc::new)
                .map_err(|error| LoadError::SoundFont(Arc::new(error)))
        })
    }

    fn asset(&self) -> Option<Asset> {
        Some(self.asset.clone())
    }

    fn handle(loaded: &Self::Loaded, _: &Life) -> Self::Handle {
        Arc::clone(loaded)
    }
}
