//! What a plugin says a parameter is for ([`ControlRole`]) and which MIDI
//! message it says drives one ([`MidiAssignment`]).

use tutti_ump::{MidiChannel, U7};

use tutti_units::ParamId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// An axis of VST3's panner functions (PanPosCenterX, Y and Z).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Axis {
    /// Left to right.
    X,
    /// Front to rear.
    Y,
    /// Bottom to top.
    Z,
}

/// What a parameter is for, where the plugin says so itself.
///
/// VST3's `IParameterFunctionName` is the one source: the host asks, per unit,
/// which parameter has a function (`getParameterIDFromFunctionName`). CLAP
/// and AU declare no roles, and a role is never guessed from a parameter's
/// name. The header's other function names are readings and one-shot actions,
/// not controls, and have no variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum ControlRole {
    /// The plugin's own dry/wet blend (the DryWetMix function): `0` dry, `1` wet.
    Mix,
    /// The pan position on an axis (the PanPosCenterX, PanPosCenterY and
    /// PanPosCenterZ functions).
    Pan(Axis),
    /// The switch for low-latency operation (the LowLatencyMode function).
    LowLatency,
}

/// The MIDI message a plugin's mapping names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum MidiSource {
    /// A Control Change.
    ControlChange(U7),
    /// Channel pressure.
    ChannelPressure,
    /// Pitch bend.
    PitchBend,
    /// A program change (VST3 `kCtrlProgramChange`).
    ProgramChange,
    /// Polyphonic key pressure (VST3 `kCtrlPolyPressure`).
    PolyPressure,
}

/// Who applies the app's controller entries after
/// [`Control::set_app_midi_map`](crate::Control::set_app_midi_map).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum MidiMapApplied {
    /// The plugin applies the entries itself (AU: they are in the unit's own
    /// mapping table); the host sends it the MIDI and translates nothing.
    Plugin,
    /// The plugin cannot take the entries; the host's node translates each
    /// mapped message into a parameter change.
    Host,
}

/// One entry of the controller table a plugin declares: a MIDI message and
/// the parameter it drives.
///
/// VST3 (`IMidiMapping`) lists a parameter per channel and controller; AU
/// (`kAudioUnitProperty_AllParameterMIDIMappings`) lists mappings that name
/// a channel or any. CLAP declares none. The VST3 host applies the table the
/// plugin declares (it turns each mapped controller into a parameter change);
/// an AU applies its own table inside the unit. The app's entries are handed to
/// the plugin with [`Control::set_app_midi_map`](crate::Control::set_app_midi_map).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MidiAssignment {
    /// The channel the message arrives on; `None` for any channel.
    pub channel: Option<MidiChannel>,
    /// The message.
    pub source: MidiSource,
    /// The parameter it drives.
    pub param: ParamId,
}

/// The controller table a host applies: the `declared` entries that no `app`
/// entry shares a channel and message with, in their order, then the `app`'s
/// entries. The app's entry replaces the plugin's for the same channel and
/// message.
///
/// **Channel precedence.** For a message on channel N an entry naming channel N
/// wins over an entry for any channel (`channel: None`) with the same message,
/// whichever table either is in; among entries of one kind the later one wins.
/// The iterator keeps both entries (an any-channel one and a specific one do
/// not share a channel); the precedence applies where the table is read.
pub fn effective_midi_map<'a>(
    declared: &'a [MidiAssignment],
    app: &'a [MidiAssignment],
) -> impl Iterator<Item = MidiAssignment> + 'a {
    declared
        .iter()
        .filter(|d| {
            !app.iter()
                .any(|a| a.channel == d.channel && a.source == d.source)
        })
        .chain(app)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(channel: Option<u8>, controller: u8, param: u32) -> MidiAssignment {
        MidiAssignment {
            channel: channel.map(MidiChannel::of),
            source: MidiSource::ControlChange(U7::of(controller)),
            param: ParamId::new(param),
        }
    }

    /// **The app's entry replaces the declared one for its channel and message;
    /// the declared entries it leaves come first, in their order.**
    #[test]
    fn the_effective_map_is_the_declared_entries_the_app_leaves_then_the_apps() {
        let declared = [
            entry(Some(0), 74, 1),
            entry(Some(1), 74, 1),
            entry(Some(0), 1, 1),
        ];
        let app = [entry(Some(1), 74, 2), entry(None, 7, 3)];
        let got: Vec<_> = effective_midi_map(&declared, &app).collect();
        assert_eq!(
            got,
            [
                entry(Some(0), 74, 1),
                entry(Some(0), 1, 1),
                entry(Some(1), 74, 2),
                entry(None, 7, 3),
            ]
        );
    }
}
