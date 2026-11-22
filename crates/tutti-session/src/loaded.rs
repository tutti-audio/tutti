//! What every loaded handle shares: the asset with the identity it was read
//! under, and a load's panic caught as an error.

#![cfg_attr(
    not(any(
        all(feature = "sampler", codec),
        feature = "soundfont",
        feature = "plugin"
    )),
    expect(
        dead_code,
        reason = "the loads (features `sampler`, `soundfont`, `plugin`) are its users"
    )
)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tutti_platform::{Asset, Identity};

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

/// Why a load did not finish.
///
/// Each load adds its own failures (a plugin's, a SoundFont's) to the common
/// ones here. Clone, so a handle's failed state and its load's error carry the
/// same value.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    /// A file could not be decoded.
    #[error(transparent)]
    Decode(Arc<tutti_codec::DecodeError>),
    /// A file's header could not be read to stream it.
    #[cfg(all(feature = "sampler", codec))]
    #[error(transparent)]
    Probe(Arc<tutti_sampler::SampleProbeError>),
    /// An asset declared streamed cannot be streamed by this build: its
    /// container reports no length, its decoder cannot seek, or no codec covers
    /// it. Carries the asset's [`name`](tutti_platform::Asset::name).
    #[cfg(feature = "sampler")]
    #[error("{0} cannot be streamed")]
    NotStreamable(String),
    /// A MIDI file could not be read.
    #[cfg(feature = "midi")]
    #[error(transparent)]
    Midi(Arc<tutti_midi_file::ReadError>),
    /// A file could not be opened to stream it, or is not audio this build
    /// decodes.
    #[cfg(feature = "sampler")]
    #[error(transparent)]
    Stream(Arc<tutti_sampler::OpenError>),
    /// A SoundFont could not be opened.
    #[cfg(feature = "soundfont")]
    #[error(transparent)]
    SoundFont(Arc<tutti_sf2::Error>),
    /// A plugin did not open.
    #[cfg(feature = "plugin")]
    #[error(transparent)]
    Plugin(Arc<tutti_plugin_types::IpcError>),
    /// The plugin is in a bundle that holds no module this target can load.
    #[cfg(feature = "plugin")]
    #[error(transparent)]
    Module(Arc<tutti_plugin_types::PluginError>),
    /// The plugin's id names no plugin of the catalog, or several.
    #[cfg(feature = "plugin")]
    #[error(transparent)]
    Unresolved(tutti_plugin_db::BindError),
    /// A load panicked (a decoder, a parser); `message` is the panic's, when it
    /// was a string.
    #[error("a load panicked: {message}")]
    Panicked {
        /// The panic's message, or a placeholder for a payload of another type.
        message: String,
    },
    /// The session was dropped before the load could run: its executor was gone,
    /// refused the task, or dropped it before it ran. The file did not fail.
    #[error("the session shut down before the load ran")]
    ShutDown,
}

#[cfg(any(feature = "sampler", feature = "midi", feature = "soundfont"))]
impl crate::unblocked::sealed::Sealed for LoadError {}

#[cfg(any(feature = "sampler", feature = "midi", feature = "soundfont"))]
impl crate::unblocked::Unanswered for LoadError {
    fn shut_down() -> Self {
        Self::ShutDown
    }

    fn panicked(message: String) -> Self {
        Self::Panicked { message }
    }
}

/// Runs `read`, a panic in it (a decoder, a parser) becoming
/// [`LoadError::Panicked`].
pub(crate) fn caught<T>(read: impl FnOnce() -> Result<T, LoadError>) -> Result<T, LoadError> {
    caught_by(read, |message| LoadError::Panicked { message })
}

/// Runs `work`, a panic in it becoming the error `panicked` makes of the panic's
/// message.
pub(crate) fn caught_by<T, E>(
    work: impl FnOnce() -> Result<T, E>,
    panicked: impl FnOnce(String) -> E,
) -> Result<T, E> {
    catch_unwind(AssertUnwindSafe(work))
        .unwrap_or_else(|payload| Err(panicked(tutti_platform::panic_message(payload.as_ref()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A panic in a load is `Panicked`, carrying the panic's message**: a
    /// decoder, a parser or a plugin open that panics fails its load and leaves
    /// the app's thread running.
    #[test]
    fn a_panic_in_a_load_is_a_panicked_error_with_its_message() {
        let error = caught::<()>(|| panic!("a decoder gave up")).expect_err("it panicked");
        assert!(
            matches!(&error, LoadError::Panicked { message } if message == "a decoder gave up"),
            "{error:?}"
        );
    }

    /// **A load that returns is passed through, value and error alike.**
    #[test]
    fn a_load_that_returns_is_passed_through() {
        assert_eq!(caught(|| Ok(7)).expect("a value"), 7);
        let own = || LoadError::Panicked {
            message: "from the load".into(),
        };
        let error = caught::<()>(|| Err(own())).expect_err("an error");
        assert!(matches!(error, LoadError::Panicked { message } if message == "from the load"));
    }
}
