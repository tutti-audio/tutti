//! The data entries: a file or a SoundFont read whole, loaded once, shared by
//! its `Arc`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::pool::entry::{Declared, Life, Load, LoadError, PoolEntry};

/// A wave file: [`audio_file`]'s entry, decoded whole (resident) or probed for
/// a disk stream ([`streamed`](Declared::streamed)).
///
/// Its identity is the path and the tier ([`Tier`]).
#[cfg(all(feature = "sampler", codec))]
#[derive(Clone, Debug)]
pub struct AudioFile {
    path: PathBuf,
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
    /// decoded size (`frames * channels * 4` bytes of `f32`) is more than
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
pub enum AudioEntry {
    /// The file, decoded whole; every handle shares the `Arc`.
    Resident(Arc<tutti_codec::Wave>),
    /// The file's header, read; its samples stay on disk until a stream opens
    /// ([`Session::open_stream`](crate::Session::open_stream) over
    /// [`StreamInfo::path`]).
    Streamed(Arc<StreamInfo>),
}

/// What a streamed [`audio_file`] entry's header says: the file and what the
/// sampler can do with it.
#[cfg(all(feature = "sampler", codec))]
#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    path: PathBuf,
    facts: tutti_sampler::SampleFacts,
}

#[cfg(all(feature = "sampler", codec))]
impl StreamInfo {
    /// The file the entry declared.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
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
    /// `f32` samples, 95 seconds of 44.1 kHz stereo.
    ///
    /// A resident wave costs its decoded size for as long as it lives, and a
    /// session holds one per file in use; a stream costs a ring and a decode
    /// on the streamer's thread whatever the file's length. Samples, loops and
    /// stems of a minute or so sit under the line, where a wave in memory seeks
    /// and loops with no read behind it; a song or a long take sits over it.
    pub const STREAM_ABOVE_BYTES: u64 = 32 * 1024 * 1024;
}

/// Declares the wave file at `path` under `key`, decoded whole; the entry reads
/// as [`AudioEntry::Resident`]. Another key declared with the same path shares
/// the one decode, and every handle is the same `Arc`.
#[cfg(all(feature = "sampler", codec))]
pub fn audio_file<K>(key: K, path: impl Into<PathBuf>) -> Declared<K, AudioFile> {
    Declared::shared(
        key,
        AudioFile {
            path: path.into(),
            tier: Tier::Resident,
        },
    )
}

#[cfg(all(feature = "sampler", codec))]
impl<K> Declared<K, AudioFile> {
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
impl PoolEntry for AudioFile {
    type Identity = (PathBuf, Tier);
    type Loaded = AudioEntry;
    type Handle = AudioEntry;

    fn identity(&self) -> Self::Identity {
        (self.path.clone(), self.tier)
    }

    fn load(&self) -> Load<Self::Loaded> {
        let (path, tier) = (self.path.clone(), self.tier);
        Load::on_worker(move || match tier {
            Tier::Resident => resident(path),
            Tier::Streamed => {
                let facts = probe(&path)?;
                if facts.streamable {
                    Ok(streamed(path, facts))
                } else {
                    Err(LoadError::NotStreamable(path))
                }
            }
            Tier::Auto { above } => {
                let facts = probe(&path)?;
                let bytes = facts.frames.map_or(0, |frames| {
                    frames.get() as u64 * u64::from(facts.layout.count()) * 4
                });
                if facts.streamable && bytes > above {
                    Ok(streamed(path, facts))
                } else {
                    resident(path)
                }
            }
        })
    }

    fn file(&self) -> Option<&std::path::Path> {
        Some(&self.path)
    }

    fn handle(loaded: &Self::Loaded, _: &Life) -> Self::Handle {
        loaded.clone()
    }
}

#[cfg(all(feature = "sampler", codec))]
fn probe(path: &std::path::Path) -> Result<tutti_sampler::SampleFacts, LoadError> {
    tutti_sampler::probe(path).map_err(|error| LoadError::Probe(Arc::new(error)))
}

#[cfg(all(feature = "sampler", codec))]
fn streamed(path: PathBuf, facts: tutti_sampler::SampleFacts) -> AudioEntry {
    AudioEntry::Streamed(Arc::new(StreamInfo { path, facts }))
}

#[cfg(all(feature = "sampler", codec))]
fn resident(path: PathBuf) -> Result<AudioEntry, LoadError> {
    tutti_codec::Wave::read(path)
        .map(|wave| AudioEntry::Resident(Arc::new(wave)))
        .map_err(|error| LoadError::Decode(Arc::new(error)))
}

/// A SoundFont file, parsed: [`soundfont`]'s entry.
///
/// Its identity is the path.
#[cfg(feature = "soundfont")]
#[derive(Clone, Debug)]
pub struct SoundFontFile {
    path: PathBuf,
}

/// Declares the SoundFont at `path` under `key`. Another key declared with the
/// same path shares the one parse, and every handle is the same `Arc`.
#[cfg(feature = "soundfont")]
pub fn soundfont<K>(key: K, path: impl Into<PathBuf>) -> Declared<K, SoundFontFile> {
    Declared::shared(key, SoundFontFile { path: path.into() })
}

#[cfg(feature = "soundfont")]
impl PoolEntry for SoundFontFile {
    type Identity = PathBuf;
    type Loaded = Arc<tutti_sf2::SoundFont>;
    type Handle = Arc<tutti_sf2::SoundFont>;

    fn identity(&self) -> PathBuf {
        self.path.clone()
    }

    fn load(&self) -> Load<Self::Loaded> {
        let path = self.path.clone();
        Load::on_worker(move || {
            tutti_sf2::SoundFont::open(path)
                .map(Arc::new)
                .map_err(|error| LoadError::SoundFont(Arc::new(error)))
        })
    }

    fn file(&self) -> Option<&std::path::Path> {
        Some(&self.path)
    }

    fn handle(loaded: &Self::Loaded, _: &Life) -> Self::Handle {
        Arc::clone(loaded)
    }
}
