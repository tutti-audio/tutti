//! [`ValueError`], the one error of every value call.

use thiserror::Error;

use super::AnyTarget;

/// Why a value call (`set`, `get`) did not go through; nothing landed.
///
/// `at` is the position of the failing item in the call's argument, flattened
/// in order with an `Option`'s `None` counted, and the first failing item's
/// when several fail. It is 0 for [`ValueErrorKind::Closed`] and
/// [`ValueErrorKind::Reentrant`], which are the whole call's. `target` names
/// the item's key and kind when the call had one to name.
///
/// `K` is the session's key type: [`Values<K>`](crate::Values) and
/// [`Session<K>`](crate::Session) answer a `ValueError<K>`.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("value {at}: {kind}")]
pub struct ValueError<K> {
    /// The failing item's position in the call's argument.
    pub at: usize,
    /// The failing item's target.
    pub target: Option<AnyTarget<K>>,
    /// What went wrong.
    pub kind: ValueErrorKind,
}

/// What went wrong in a value call ([`ValueError::kind`]).
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ValueErrorKind {
    /// The target's node is not in the session's graph, or does not have the
    /// param.
    #[error("no such target")]
    NoSuchTarget,
    /// The session is gone: every owner of its value state has dropped.
    #[error("the session is gone")]
    Closed,
    /// The call was made from inside a `modify` closure, which holds the
    /// value lock.
    #[error("called from inside a modify")]
    Reentrant,
    /// The target refused the value.
    #[error(transparent)]
    Refused(#[from] SetRefusal),
}

/// Why a target refused a value ([`ValueErrorKind::Refused`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SetRefusal {
    /// A float target was given a NaN or an infinity.
    #[error("the value is not a finite number")]
    NotFinite,
    /// A finite value outside the range the target takes.
    #[error("the value is outside the target's range")]
    OutOfRange,
    /// A range with its start above its end.
    #[error("the range is backwards: its start is above its end")]
    Backwards,
    /// A node's mode or mix needs a pass, and the node has none: its buses do
    /// not say where its input goes when it does not run, so a bypass or a
    /// blend would be a guess.
    #[error(transparent)]
    Unplaced(#[from] tutti_graph::PassError),
    /// The session follows a live tempo, which owns the tempo map: an app's
    /// write would be overwritten at the next message.
    #[error("a live tempo is followed")]
    Following,
    /// A plugin's parameter refused the write.
    #[cfg(feature = "plugin")]
    #[error(transparent)]
    Plugin(#[from] PluginParamRefusal),
    /// The output policy has no place on the root and the device.
    #[error(transparent)]
    OutputPolicy(#[from] crate::OutputPolicyError),
    /// The MIDI policy has no place on the session's MIDI layer.
    #[cfg(feature = "midi-hardware")]
    #[error(transparent)]
    MidiPolicy(#[from] crate::MidiPolicyError),
}

/// Why a plugin's parameter refused a write ([`SetRefusal::Plugin`]).
#[cfg(feature = "plugin")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PluginParamRefusal {
    /// The plugin's parameters have not been read yet: the session reads them
    /// after the plugin loads and lands the read in a later update.
    #[error("the plugin's parameters have not been read yet")]
    NotRead,
    /// The plugin declares no parameter with this id.
    #[error(transparent)]
    UnknownParam(#[from] tutti_plugin_types::UnknownParam),
    /// The parameter is declared read-only.
    #[error("parameter {0} is read-only")]
    ReadOnly(tutti_units::ParamId),
}
