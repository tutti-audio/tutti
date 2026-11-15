//! The data entries: a file or a SoundFont read whole, loaded once, shared by
//! its `Arc`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::pool::entry::{Declared, Life, Load, LoadError, PoolEntry};

/// A wave file, decoded whole: [`audio_file`]'s entry.
///
/// Its identity is the path. A streamed tier and a content stamp, which the
/// pool's design adds to a file's identity, are not here yet.
#[cfg(feature = "sampler")]
#[derive(Clone, Debug)]
pub struct AudioFile {
    path: PathBuf,
}

/// Declares the wave file at `path` under `key`.
#[cfg(feature = "sampler")]
pub fn audio_file<K>(key: K, path: impl Into<PathBuf>) -> Declared<K, AudioFile> {
    Declared::new(key, AudioFile { path: path.into() })
}

#[cfg(feature = "sampler")]
impl PoolEntry for AudioFile {
    type Identity = PathBuf;
    type Loaded = Arc<tutti_codec::Wave>;
    type Handle = Arc<tutti_codec::Wave>;

    fn identity(&self) -> PathBuf {
        self.path.clone()
    }

    fn load(&self) -> Load<Self::Loaded> {
        let path = self.path.clone();
        Load::on_worker(move || {
            tutti_codec::Wave::read(path)
                .map(Arc::new)
                .map_err(|error| LoadError::Decode(Arc::new(error)))
        })
    }

    fn handle(loaded: &Self::Loaded, _: &Life) -> Self::Handle {
        Arc::clone(loaded)
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

/// Declares the SoundFont at `path` under `key`.
#[cfg(feature = "soundfont")]
pub fn soundfont<K>(key: K, path: impl Into<PathBuf>) -> Declared<K, SoundFontFile> {
    Declared::new(key, SoundFontFile { path: path.into() })
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

    fn handle(loaded: &Self::Loaded, _: &Life) -> Self::Handle {
        Arc::clone(loaded)
    }
}
