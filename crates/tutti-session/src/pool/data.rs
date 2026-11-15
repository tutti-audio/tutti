//! The data entries: a file or a SoundFont read whole, loaded once, shared by
//! its `Arc`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::pool::entry::{Declared, Life, Load, LoadError, PoolEntry};

/// A wave file: [`audio_file`]'s entry, decoded whole (resident) or probed for
/// a disk stream ([`streamed`](Declared::streamed)).
///
/// Its identity is the path and the tier.
#[cfg(feature = "sampler")]
#[derive(Clone, Debug)]
pub struct AudioFile {
    path: PathBuf,
    tier: Tier,
}

/// Where a file's samples are read from: half of [`AudioFile`]'s identity.
#[cfg(feature = "sampler")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Decoded whole into memory.
    Resident,
    /// Read from disk by a stream.
    Streamed,
}

/// What a landed [`audio_file`] entry reads as: the decoded wave of a resident
/// entry, or what a streamed entry's header says.
///
/// A handle is a clone and owns nothing: dropping it leaves the entry loaded.
#[cfg(feature = "sampler")]
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
#[cfg(feature = "sampler")]
#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    path: PathBuf,
    facts: tutti_sampler::SampleFacts,
}

#[cfg(feature = "sampler")]
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

/// Declares the wave file at `path` under `key`, decoded whole; the entry reads
/// as [`AudioEntry::Resident`]. Another key declared with the same path shares
/// the one decode, and every handle is the same `Arc`.
#[cfg(feature = "sampler")]
pub fn audio_file<K>(key: K, path: impl Into<PathBuf>) -> Declared<K, AudioFile> {
    Declared::shared(
        key,
        AudioFile {
            path: path.into(),
            tier: Tier::Resident,
        },
    )
}

#[cfg(feature = "sampler")]
impl<K> Declared<K, AudioFile> {
    /// The file played from disk: its entry reads as [`AudioEntry::Streamed`]
    /// and fails with [`LoadError::NotStreamable`] for a file this build cannot
    /// stream. The tier is part of the identity: changing it reloads.
    pub fn streamed(mut self) -> Self {
        self.entry.tier = Tier::Streamed;
        self
    }
}

#[cfg(feature = "sampler")]
impl PoolEntry for AudioFile {
    type Identity = (PathBuf, Tier);
    type Loaded = AudioEntry;
    type Handle = AudioEntry;

    fn identity(&self) -> Self::Identity {
        (self.path.clone(), self.tier)
    }

    fn load(&self) -> Load<Self::Loaded> {
        let path = self.path.clone();
        match self.tier {
            Tier::Resident => Load::on_worker(move || {
                tutti_codec::Wave::read(path)
                    .map(|wave| AudioEntry::Resident(Arc::new(wave)))
                    .map_err(|error| LoadError::Decode(Arc::new(error)))
            }),
            Tier::Streamed => Load::on_worker(move || {
                let facts = tutti_sampler::probe(&path)
                    .map_err(|error| LoadError::Probe(Arc::new(error)))?;
                if !facts.streamable {
                    return Err(LoadError::NotStreamable(path));
                }
                Ok(AudioEntry::Streamed(Arc::new(StreamInfo { path, facts })))
            }),
        }
    }

    fn file(&self) -> Option<&std::path::Path> {
        Some(&self.path)
    }

    fn handle(loaded: &Self::Loaded, _: &Life) -> Self::Handle {
        loaded.clone()
    }
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
