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

/// One group of parameters: the label [`ParameterInfo::group`](crate::ParameterInfo::group)
/// carries, and the group that holds it.
///
/// VST3's units are a tree, and CLAP's `module` is a `/`-separated path whose
/// prefixes are the ancestors; VST2 and AU have no tree and declare none.
/// A parent is named by its `name`, as a parameter names its group.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ParamGroup {
    /// The group's label: what its parameters' `group` reads.
    pub name: String,
    /// The group that holds this one; `None` at the top.
    pub parent: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two groups with one name are two entries: the list is the plugin's, and
    /// a reader resolves a parent by the first match, in list order.
    #[test]
    fn a_group_is_named_by_its_label() {
        let top = ParamGroup {
            name: "Voices".into(),
            parent: None,
        };
        let child = ParamGroup {
            name: "Filter".into(),
            parent: Some(top.name.clone()),
        };
        assert_ne!(top, child);
        assert_eq!(child.parent.as_deref(), Some("Voices"));
    }
}
