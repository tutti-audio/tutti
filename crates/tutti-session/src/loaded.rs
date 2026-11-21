//! What every loaded handle shares: the asset with the identity it was read
//! under, and a load's panic caught as an error.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_platform::{Asset, Identity};

use crate::LoadError;
use crate::jobs::panic_message;

/// An asset and its [`Identity`] as taken before a load read it.
#[derive(Clone, Debug)]
pub(crate) struct Stamp {
    asset: Asset,
    loaded_from: Option<Identity>,
}

impl Stamp {
    /// The stamp of `asset` now: take it before the read, so a rewrite during
    /// the read shows in [`changed`](Self::changed).
    pub(crate) fn take(asset: Asset) -> Self {
        let loaded_from = asset.identity();
        Self { asset, loaded_from }
    }

    pub(crate) const fn asset(&self) -> &Asset {
        &self.asset
    }

    /// Whether the asset is not what was read: its [`Identity`] (a file's path
    /// with its length and modification time) now differs from the one taken
    /// before the load. This reads the file's metadata, and nothing else.
    ///
    /// An asset of bytes never changes. A file stamped at the load reads
    /// changed when it is gone or differs; one that could not be stamped reads
    /// changed once it can be.
    pub(crate) fn changed(&self) -> bool {
        self.asset.identity() != self.loaded_from
    }
}

/// Runs `read`, a panic in it (a decoder, a parser) becoming
/// [`LoadError::Panicked`].
pub(crate) fn caught<T>(read: impl FnOnce() -> Result<T, LoadError>) -> Result<T, LoadError> {
    catch_unwind(AssertUnwindSafe(read)).unwrap_or_else(|payload| {
        Err(LoadError::Panicked {
            message: panic_message(payload.as_ref()),
        })
    })
}
