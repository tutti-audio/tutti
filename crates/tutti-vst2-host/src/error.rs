//! Error types for VST2 plugin hosting.

use std::path::PathBuf;
use thiserror::Error;

/// Convenience alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, Vst2Error>;

/// The phase a plugin load reached before failing, carried by
/// [`Vst2Error::LoadFailed`].
///
/// Shared by all format hosts; VST2 reports only the Opening, Factory,
/// Instantiation and Initialization phases.
pub use tutti_plugin_types::LoadStage;
use tutti_plugin_types::StateFrameError;
use vst::api::GetChunkError;
use vst::host::PluginLoadError;

/// An error reported by the VST2 host.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Vst2Error {
    /// The plugin could not be loaded. `stage` says how far the load got, which
    /// separates "the file is not a VST2 binary" from "the plugin loaded and
    /// then refused to initialise".
    #[error("failed to load the plugin at {}: {stage} - {reason}", path.display())]
    LoadFailed {
        /// The binary the load was attempted against.
        path: PathBuf,
        /// The phase that failed: Opening, Factory, Instantiation or
        /// Initialization for VST2.
        stage: LoadStage,
        /// Human-readable cause, for logs rather than for matching on.
        reason: String,
    },

    /// The plugin loader refused the binary.
    #[error("failed to load the plugin at {}: {stage}", path.display())]
    Loader {
        /// The binary the load was attempted against.
        path: PathBuf,
        /// The phase that failed.
        stage: LoadStage,
        /// What the loader reported.
        #[source]
        source: PluginLoadError,
    },

    /// The plugin has no editor to open.
    #[error("the plugin has no editor")]
    NoEditor,

    /// The plugin's `effEditOpen` answered that the editor did not open.
    #[error("the plugin's editor did not open")]
    EditorOpenFailed,

    /// The plugin's `effGetChunk` failed although it advertises program chunks.
    #[error("the plugin advertises effFlagsProgramChunks but its chunk save failed")]
    ChunkSave(#[source] GetChunkError),

    /// The plugin reports parameters but exposes no `getParameter`, so a parameter cannot
    /// be saved.
    #[error(
        "the plugin reports {count} parameters but exposes no getParameter, so parameter {index} cannot be saved"
    )]
    NoGetParameter {
        /// The plugin's parameter count.
        count: i32,
        /// The parameter that could not be read.
        index: i32,
    },

    /// The saved state holds more parameters than the plugin has.
    #[error("the saved state holds {saved} parameters but the plugin has {actual}")]
    TooManyParameters {
        /// The count the state holds.
        saved: i32,
        /// The count the plugin has.
        actual: i32,
    },

    /// The plugin exposes no `setParameter`, so a parameter cannot be restored.
    #[error("the plugin exposes no setParameter, so parameter {index} cannot be restored")]
    NoSetParameter {
        /// The parameter that could not be written.
        index: i32,
    },

    /// A saved state is not one this host's container reads: untagged, or tagged and cut
    /// short or malformed. Which, is the [`StateFrameError`] variant.
    #[error("State error: {0}")]
    StateFrame(#[from] StateFrameError),
}
