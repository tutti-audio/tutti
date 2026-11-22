//! A SoundFont as a value: [`Session::load_soundfont`] returns a
//! [`LoadSoundFont`] that, when the app awaits it, parses the file and resolves
//! to a [`LoadedSoundFont`] handle.
//!
//! A load holds the asset and the session's weak executor, and the session keeps
//! nothing of what it loads: two loads of one file are two parses, and the font
//! lives while a clone of the handle does.

use std::future::IntoFuture;
use std::sync::Arc;

use tutti_platform::{Asset, WeakExecutor};

use crate::loaded::{Stamp, caught};
use crate::unblocked::Unblocked;
use crate::{AppKey, Clock, LoadError, Session};

/// A SoundFont to parse, built by [`Session::load_soundfont`]. It is `Send`, and
/// it is a [`Future`] through [`IntoFuture`]: awaiting it parses the file as a
/// task on the session's executor and resolves to a [`LoadedSoundFont`]. There is
/// no blocking form.
///
/// It holds the asset and a weak handle to the session's executor, and nothing
/// else of the session. Dropping it unawaited reads nothing.
#[derive(Debug)]
#[must_use = "a load does nothing unless it is awaited"]
pub struct LoadSoundFont {
    asset: Asset,
    executor: WeakExecutor,
}

impl LoadSoundFont {
    /// Parses the file. The asset's [`Identity`](tutti_platform::Identity) is taken
    /// before the read, so a rewrite during it shows in [`LoadedSoundFont::changed`].
    fn run(self) -> Result<LoadedSoundFont, LoadError> {
        let stamp = Stamp::take(self.asset.clone());
        let font = caught(|| {
            tutti_sf2::SoundFont::open(self.asset)
                .map_err(|error| LoadError::SoundFont(Arc::new(error)))
        })?;
        Ok(LoadedSoundFont {
            stamp,
            font: Arc::new(font),
        })
    }
}

impl IntoFuture for LoadSoundFont {
    type Output = Result<LoadedSoundFont, LoadError>;
    type IntoFuture = LoadingSoundFont;

    fn into_future(self) -> LoadingSoundFont {
        Unblocked::new(self.executor.clone(), self, Self::run)
    }
}

/// A SoundFont load being awaited ([`LoadSoundFont`]): resolves to the
/// [`LoadedSoundFont`] once the file is parsed.
///
/// A [`Unblocked`]: its first poll starts the work as a task on the session's executor, and
/// `Session::update` is not needed.
///
/// # Errors
///
/// The output is [`LoadError::SoundFont`] for a file that is no SoundFont;
/// [`LoadError::Panicked`] when the parser panicked; [`LoadError::ShutDown`]
/// when the session is gone.
///
/// # Panics
///
/// When polled after it resolved.
pub type LoadingSoundFont = Unblocked<LoadSoundFont, LoadedSoundFont, LoadError>;

/// A parsed SoundFont. A clone shares the font; releasing is dropping every
/// clone.
#[derive(Clone, Debug)]
pub struct LoadedSoundFont {
    stamp: Stamp,
    font: Arc<tutti_sf2::SoundFont>,
}

impl LoadedSoundFont {
    /// The parsed font, shared by its `Arc`: what a `SoundFontUnit` is built
    /// from.
    #[must_use]
    pub const fn font(&self) -> &Arc<tutti_sf2::SoundFont> {
        &self.font
    }

    /// The asset the load read.
    #[must_use]
    pub const fn asset(&self) -> &Asset {
        self.stamp.asset()
    }

    /// Whether the asset is not what was read: its [`Identity`](tutti_platform::Identity) now differs from
    /// the one taken before the load. This reads the file's metadata, and
    /// nothing else.
    ///
    /// An asset of bytes never changes. A file stamped at the load reads
    /// changed when it is gone or differs; one that could not be stamped reads
    /// changed once it can be. To reload, await [`Session::load_soundfont`] over
    /// [`asset`](Self::asset).
    #[must_use]
    pub fn changed(&self) -> bool {
        self.stamp.changed()
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// A SoundFont to load, as a value the app awaits.
    pub fn load_soundfont(&self, asset: impl Into<Asset>) -> LoadSoundFont {
        LoadSoundFont {
            asset: asset.into(),
            executor: self.background.executor().downgrade(),
        }
    }
}
