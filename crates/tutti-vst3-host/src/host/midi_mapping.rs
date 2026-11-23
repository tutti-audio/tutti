//! VST3 `IMidiMapping` support: reading the plugin's declared controller table.
//!
//! Many VST3 instruments expose mod-wheel, breath, expression, sustain, etc.
//! *only* through `IMidiMapping::getMidiControllerAssignment` rather than
//! reacting to raw CC events. At each activation and after
//! `kMidiCCAssignmentChanged` (UI thread) the host walks every channel ×
//! controller-number slot and asks the controller which parameter, if any, that
//! controller drives; the result is the plugin's declared
//! [`MidiAssignment`]s
//! ([`Control::wiring`](tutti_plugin_types::Control::wiring)).
//!
//! The host does not apply the table here: the plugin node translates each
//! mapped controller into a parameter change (`tutti-plugin-host`'s controller
//! map), and the processor receives the MIDI that nothing mapped.
//!
//! Spec reference: `Steinberg::Vst::IMidiMapping`,
//! `Steinberg::Vst::ControllerNumbers`.

use vst3::Steinberg::Vst::{
    ControllerNumbers_::{kAfterTouch, kCountCtrlNumber, kPitchBend},
    ParamID,
};

use tutti_midi_protocol::channel_voice;
use tutti_plugin_types::{MainThreadMarker, MidiAssignment, MidiSource};
use tutti_ump::{Address, MidiChannel, MidiGroup, U7, Ump, Voice2};
use tutti_units::ParamId;
use tutti_vst3_ffi::Plugin;

/// Number of MIDI channels VST3 enumerates mappings for.
const NUM_CHANNELS: usize = 16;

/// Number of distinct controller slots per channel: CC 0-127 plus the two
/// synthetic controllers (`kAfterTouch` = 128, `kPitchBend` = 129).
/// Equal to `Steinberg::Vst::kCountCtrlNumber`: [`scan`] asks for exactly these
/// slots.
const NUM_CONTROLLERS: usize = kCountCtrlNumber as usize;

/// Sentinel `ParamID` meaning "no parameter mapped". Mirrors
/// `Steinberg::Vst::kNoParamId` (`0xFFFF_FFFF`).
const NO_PARAM_ID: ParamID = u32::MAX;

/// Synthetic controller index VST3 uses for channel aftertouch.
const CTRL_AFTERTOUCH: usize = kAfterTouch as usize;
/// Synthetic controller index VST3 uses for pitch bend.
const CTRL_PITCH_BEND: usize = kPitchBend as usize;

/// One slot of the plugin's `IMidiMapping`: the channel and controller number
/// it was asked for, and the parameter it names.
#[derive(Clone, Copy)]
pub(crate) struct Slot {
    channel: i16,
    controller: i16,
    id: ParamID,
}

/// Asks the plugin's `IMidiMapping` (event bus 0, the JUCE host contract) for
/// every channel and every controller number `IMidiMapping` addresses
/// (the controller numbers below `kCountCtrlNumber`): the Control Changes, channel
/// pressure and pitch bend. Program change and polyphonic pressure are not
/// among them (the SDK defines those numbers for `kLegacyMIDICCOutEvent` only)
/// and stay MIDI events. Empty without an `IMidiMapping`. Main thread.
pub(crate) fn scan(main: MainThreadMarker, plugin: &Plugin) -> Vec<Slot> {
    let mut slots = Vec::new();
    if !plugin.has_midi_mapping(main) {
        return slots;
    }
    plugin.midi_controller_assignments(
        main,
        0..NUM_CHANNELS as i16,
        0..NUM_CONTROLLERS as i16,
        |channel, controller, id| {
            if id != NO_PARAM_ID {
                slots.push(Slot {
                    channel,
                    controller,
                    id,
                });
            }
        },
    );
    slots
}

/// The slots as the plugin's declared assignments, one per mapped channel and
/// message.
pub(crate) fn declared_assignments(slots: &[Slot]) -> Vec<MidiAssignment> {
    slots
        .iter()
        .filter_map(|slot| {
            let source = match slot.controller {
                c if c == CTRL_AFTERTOUCH as i16 => MidiSource::ChannelPressure,
                c if c == CTRL_PITCH_BEND as i16 => MidiSource::PitchBend,
                cc => MidiSource::ControlChange(u8::try_from(cc).ok().and_then(U7::new)?),
            };
            Some(MidiAssignment {
                channel: u8::try_from(slot.channel).ok().and_then(MidiChannel::new),
                source,
                param: ParamId::new(slot.id),
            })
        })
        .collect()
}

/// The VST3 controller a message is, for the plugin's `IMidiLearn`: its channel
/// and `ControllerNumbers` index (0-127 for a Control Change, [`CTRL_AFTERTOUCH`]
/// for channel pressure, [`CTRL_PITCH_BEND`] for pitch bend). `None` for any other
/// message, and for a Group other than the first: the plugin's event bus 0 is the
/// one `IMidiMapping` and `IMidiLearn` address.
pub(crate) fn learnable_controller(ump: Ump) -> Option<(MidiChannel, usize)> {
    let (Address { group, channel }, voice) = channel_voice(ump)?;
    if group != MidiGroup::FIRST {
        return None;
    }
    match voice {
        Voice2::ControlChange { controller, .. } => Some((channel, usize::from(controller.get()))),
        Voice2::ChannelPressure(_) => Some((channel, CTRL_AFTERTOUCH)),
        Voice2::PitchBend(_) => Some((channel, CTRL_PITCH_BEND)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::midi;
    use tutti_ump::{CCNumber, ControlValue};

    /// **A Control Change, pressure and bend are learned as the numbers VST3
    /// gives them (CC number, 128, 129) on their channel**; a note, and a
    /// controller of another Group, are not.
    #[test]
    fn a_controller_is_learned_as_its_channel_and_vst3_number() {
        let channel = MidiChannel::of(2);
        let word = ControlValue::MAX.get();
        assert_eq!(
            learnable_controller(midi::cc(channel, CCNumber::BRIGHTNESS, word)),
            Some((channel, 74))
        );
        assert_eq!(
            learnable_controller(midi::channel_pressure(channel, word)),
            Some((channel, 128))
        );
        assert_eq!(
            learnable_controller(midi::pitch_bend(channel, word)),
            Some((channel, 129))
        );
        assert_eq!(learnable_controller(midi::note_on(channel, 60, 100)), None);
        let group_one = Address {
            group: MidiGroup::of(1),
            channel,
        };
        assert_eq!(
            learnable_controller(midi::cc(group_one, CCNumber::BRIGHTNESS, word)),
            None
        );
    }
}
