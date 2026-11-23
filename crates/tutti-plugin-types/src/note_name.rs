//! The names a plugin gives to MIDI keys ([`NoteName`]) and the groups it
//! sorts its parameters into ([`ParamGroup`]).

use tutti_ump::{MidiChannel, NoteNumber};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The plugin's name for one MIDI key: a drum pad's label, a keyswitch's
/// articulation.
///
/// CLAP's `note-name` extension, VST3's `IKeyswitchController` and VST2's
/// `effGetMidiKeyName` declare them. A name covers the first note port: the
/// bus a plugin's MIDI input feeds. A programs list is not here; it is
/// [`Control::presets`](crate::Control::presets).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteName {
    /// The key it names.
    pub key: NoteNumber,
    /// The channel it applies on; `None` for every channel.
    pub channel: Option<MidiChannel>,
    /// The name, as the plugin wrote it.
    pub text: String,
}

/// One group of parameters, identified by its path.
///
/// The path is the group's own and its ancestors' labels, root first, joined with
/// `/`; it is unique in a plugin's list and is what
/// [`ParameterInfo::group`](crate::ParameterInfo::group) reads for a parameter in
/// the group. VST3's units are a tree, and CLAP's `module` is already such a path
/// whose prefixes are the ancestors; VST2 and AU declare no tree. A label that
/// itself holds a `/` is kept as the plugin wrote it, so a path with one is
/// ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ParamGroup {
    /// The group's full path: the key.
    pub path: String,
    /// The group's own name, the last segment of `path`.
    pub label: String,
    /// The `path` of the group that holds this one; `None` at the top.
    pub parent: Option<String>,
}
