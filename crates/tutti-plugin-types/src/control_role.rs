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

/// The table a plugin's controllers follow once the app's entries are laid over
/// `declared`, the plugin's own: the declared entries no app entry shares a
/// channel and source with, in their order, then the app's entries in theirs.
///
/// An app entry wins per `(channel, source)`, where `None` (any channel) and a
/// channel are different keys. A message on a channel that matches an entry
/// naming that channel and an any-channel entry drives the one naming the
/// channel.
pub fn effective_midi_map<'a>(
    declared: &'a [MidiAssignment],
    app: &'a [MidiAssignment],
) -> impl Iterator<Item = MidiAssignment> + 'a {
    let kept = declared.iter().copied().filter(|d| {
        !app.iter()
            .any(|a| a.channel == d.channel && a.source == d.source)
    });
    kept.chain(app.iter().copied())
}
