//! [`ServerError`]: why the plugin-server failed a request it was asked, as the
//! wire carries it.

use thiserror::Error;

use crate::LoadStage;

/// Why the plugin-server failed a request.
///
/// The wire carries the arm, so a host reads what failed (the stage a load
/// reached, the sizes a state disagreed on) without parsing text. The `reason`
/// fields are the plugin's or the loader's own words, shown and never parsed.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ServerError {
    /// A loader failed the load, at `stage`.
    #[error("{stage}: {reason}")]
    Load {
        /// How far the load got.
        stage: LoadStage,
        /// The loader's own message.
        reason: String,
    },
    /// A failure of the plugin's loader that names no stage.
    #[error("{0}")]
    Plugin(String),
    /// The audio slab could not be opened.
    #[error("{reason}")]
    SlabNotOpened {
        /// Why.
        reason: String,
    },
    /// The render thread did not take the audio slab.
    #[error("the render thread did not take the slab: {reason}")]
    SlabNotTaken {
        /// Why.
        reason: String,
    },
    /// A state request arrived without its state object.
    #[error("a state arrived without its object")]
    StateWithoutObject,
    /// A slab setup arrived without its slab.
    #[error("a slab setup arrived without its slab")]
    SlabSetupWithoutSlab,
    /// A load declared one state length and the server holds another.
    #[error("the load declares a state of {declared:?} bytes and the server holds {staged:?}")]
    StateLengthMismatch {
        /// The length the load declared, if it declared a state.
        declared: Option<u64>,
        /// The length of the state staged, if one was.
        staged: Option<u64>,
    },
    /// The loaded plugin names no id of its own.
    #[error("the loaded plugin names no id of its own")]
    NoPluginId,
    /// The render thread did not accept the loaded plugin.
    #[error("the loaded plugin cannot be seated: {reason}")]
    NotSeated {
        /// Why.
        reason: String,
    },
    /// The plugin failed to save its state.
    #[error("failed to save state: {reason}")]
    SaveFailed {
        /// The plugin's own message.
        reason: String,
    },
    /// The plugin's state is over the transport's limit.
    #[error("plugin state is {bytes} bytes, over the {limit}-byte limit")]
    StateTooLarge {
        /// The state's length.
        bytes: u64,
        /// The limit.
        limit: u64,
    },
    /// The state could not be made into a shared object to send.
    #[error("the state could not be made into an object: {reason}")]
    StateNotShared {
        /// Why.
        reason: String,
    },
    /// A lane open arrived without the lane's end.
    #[error("a lane open arrived without its end")]
    LaneWithoutEnd,
    /// A lane was opened that is already open.
    #[error("the audio lane is already open")]
    LaneAlreadyOpen,
    /// A request that needs a plugin arrived with none loaded.
    #[error("no plugin is loaded")]
    NoPlugin,
}
