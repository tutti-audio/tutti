//! A SoundFont as a value: [`Session::load_soundfont`] returns a
//! [`LoadSoundFont`] that, when the app runs it, parses the file and returns a
//! [`LoadedSoundFont`] handle.
//!
//! A load holds the asset and nothing of the session, and the session keeps
//! nothing of what it loads: two loads of one file are two parses, and the font
//! lives while a clone of the handle does.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tutti_platform::{Asset, Identity};

use crate::jobs::panic_message;
use crate::{AppKey, Clock, LoadError, Session, WorkerError};

/// A SoundFont to parse, built by [`Session::load_soundfont`] and not yet run:
/// [`run`](Self::run) returns a [`LoadedSoundFont`].
///
/// It holds the asset and nothing of the session, so the app runs it on any
/// thread it chooses; dropping it unrun cancels it.
#[derive(Debug)]
#[must_use = "work does nothing until it runs"]
pub struct LoadSoundFont {
    asset: Asset,
}

impl LoadSoundFont {
    /// Parses the file on the calling thread. The asset's [`Identity`] is taken
    /// before the read, so a rewrite during it shows in [`LoadedSoundFont::changed`].
    ///
    /// # Errors
    ///
    /// [`LoadError::SoundFont`] for a file that is no SoundFont;
    /// [`LoadError::Job`] when the parser panicked.
    pub fn run(self) -> Result<LoadedSoundFont, LoadError> {
        let loaded_from = self.asset.identity();
        let font = catch_unwind(AssertUnwindSafe(|| {
            tutti_sf2::SoundFont::open(self.asset.clone())
                .map_err(|error| LoadError::SoundFont(Arc::new(error)))
        }))
        .unwrap_or_else(|payload| {
            Err(LoadError::Job(WorkerError::Panicked {
                message: panic_message(payload.as_ref()),
            }))
        })?;
        Ok(LoadedSoundFont {
            asset: self.asset,
            loaded_from,
            font: Arc::new(font),
        })
    }
}

/// A parsed SoundFont. A clone shares the font; releasing is dropping every
/// clone.
#[derive(Clone, Debug)]
pub struct LoadedSoundFont {
    asset: Asset,
    loaded_from: Option<Identity>,
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
        &self.asset
    }

    /// Whether the asset is not what was read: its [`Identity`] now differs from
    /// the one taken before the load. This reads the file's metadata, and
    /// nothing else.
    ///
    /// An asset of bytes never changes, and one that cannot be stamped now (a
    /// file that is gone) reads unchanged. To reload, run
    /// [`Session::load_soundfont`] over [`asset`](Self::asset).
    #[must_use]
    pub fn changed(&self) -> bool {
        self.asset
            .identity()
            .is_some_and(|now| self.loaded_from.as_ref() != Some(&now))
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// A SoundFont to load, as a value the app runs where it likes.
    pub fn load_soundfont(&self, asset: impl Into<Asset>) -> LoadSoundFont {
        LoadSoundFont {
            asset: asset.into(),
        }
    }
}
