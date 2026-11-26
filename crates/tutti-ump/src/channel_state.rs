//! What a channel voice message does to the state of its channel.

use crate::{CCNumber, U7, Voice1, Voice2};

/// Which kind of parameter number a [`ChannelState::Parameter`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ParameterKind {
    /// A Registered Parameter Number (MIDI 1.0 CC 101/100, MIDI 2.0 Registered
    /// Controller).
    Registered,
    /// An Assignable (Non-Registered) Parameter Number (CC 99/98, Assignable
    /// Controller).
    Assignable,
}

/// A channel mode: a command whose setting a receiver keeps, not a value a chase
/// restores. Each is one setting, the last message of its pair being the one in
/// force.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChannelMode {
    /// Local Control (CC 122).
    LocalControl,
    /// Omni Mode Off or On (CC 124, 125).
    Omni,
    /// Mono Mode On or Poly Mode On (CC 126, 127).
    MonoPoly,
}

/// One value of a channel's state: what a receiver that joins late has to be told
/// to sound as the channel does.
///
/// The derived order is the order to tell them in: the program first (a
/// controller's meaning can depend on the patch), the parameters (a bend range
/// among them) before the bend they scale, then the controllers, the pressure and
/// the bend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChannelState {
    /// The program (MIDI 2.0: with its bank).
    Program,
    /// A registered or assignable parameter, by its bank (MSB) and index (LSB).
    Parameter(ParameterKind, U7, U7),
    /// A controller's value.
    Controller(CCNumber),
    /// The channel pressure.
    Pressure,
    /// The pitch bend.
    Bend,
}

/// What a channel voice message does to its channel's state: the one
/// classification a record of what a stream said and a clip's chase share.
///
/// Notes, polyphonic pressure and the per-note and relative controllers are not
/// channel state ([`of_midi1`](Self::of_midi1) and [`of_midi2`](Self::of_midi2)
/// answer `None`): a per-note controller ends with its note (M2-104 section
/// 7.4) and a relative one adds to what is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelEffect {
    /// Sets an absolute value of the channel's state.
    Sets(ChannelState),
    /// A mode message (Local Control, Omni Off, Omni On, Mono On, Poly On): a
    /// command, not a value to restore, whose setting a receiver still keeps.
    /// Omni, Mono and Poly also end the channel's notes
    /// ([`ends_notes`](Self::ends_notes)).
    Mode(ChannelMode),
    /// All Sound Off (CC 120) or All Notes Off (CC 123): ends the channel's notes
    /// and sets nothing.
    EndsNotes,
    /// Reset All Controllers (CC 121): returns what RP-015 lists to its normal
    /// position.
    ResetsControllers,
}

impl ChannelEffect {
    /// What a MIDI 1.0 channel voice message does, or `None` when it is no
    /// channel state. A bank select, a parameter select and a data entry are
    /// controllers here: pairing them is the caller's.
    #[must_use]
    pub fn of_midi1(voice: Voice1) -> Option<Self> {
        match voice {
            Voice1::ControlChange { controller, .. } => Self::of_controller(controller),
            Voice1::ProgramChange(_) => Some(Self::Sets(ChannelState::Program)),
            Voice1::ChannelPressure(_) => Some(Self::Sets(ChannelState::Pressure)),
            Voice1::PitchBend(_) => Some(Self::Sets(ChannelState::Bend)),
            Voice1::NoteOn { .. } | Voice1::NoteOff { .. } | Voice1::PolyPressure { .. } => None,
        }
    }

    /// What a MIDI 2.0 channel voice message does, or `None` when it is no
    /// channel state. The controllers a MIDI 2.0 stream ignores
    /// ([`CCNumber::is_ignored_in_midi2`]) are none.
    #[must_use]
    pub fn of_midi2(voice: Voice2) -> Option<Self> {
        let parameter =
            |kind, bank, index| Some(Self::Sets(ChannelState::Parameter(kind, bank, index)));
        match voice {
            Voice2::ControlChange { controller, .. } if !controller.is_ignored_in_midi2() => {
                Self::of_controller(controller)
            }
            Voice2::ProgramChange { .. } => Some(Self::Sets(ChannelState::Program)),
            Voice2::ChannelPressure(_) => Some(Self::Sets(ChannelState::Pressure)),
            Voice2::PitchBend(_) => Some(Self::Sets(ChannelState::Bend)),
            Voice2::Registered { bank, index, .. } => {
                parameter(ParameterKind::Registered, bank, index)
            }
            Voice2::Assignable { bank, index, .. } => {
                parameter(ParameterKind::Assignable, bank, index)
            }
            _ => None,
        }
    }

    fn of_controller(controller: CCNumber) -> Option<Self> {
        Some(match controller {
            CCNumber::ALL_SOUND_OFF | CCNumber::ALL_NOTES_OFF => Self::EndsNotes,
            CCNumber::RESET_ALL => Self::ResetsControllers,
            CCNumber::LOCAL_CONTROL => Self::Mode(ChannelMode::LocalControl),
            CCNumber::OMNI_MODE_OFF | CCNumber::OMNI_MODE_ON => Self::Mode(ChannelMode::Omni),
            CCNumber::MONO_MODE_ON | CCNumber::POLY_MODE_ON => Self::Mode(ChannelMode::MonoPoly),
            // Events, not values.
            CCNumber::PORTAMENTO_CONTROL
            | CCNumber::VELOCITY_PREFIX
            | CCNumber::DATA_INCREMENT
            | CCNumber::DATA_DECREMENT => return None,
            _ => Self::Sets(ChannelState::Controller(controller)),
        })
    }

    /// Whether the message ends the channel's notes: All Sound Off, All Notes
    /// Off, and the Omni, Mono and Poly modes.
    #[must_use]
    pub const fn ends_notes(self) -> bool {
        matches!(
            self,
            Self::EndsNotes | Self::Mode(ChannelMode::Omni | ChannelMode::MonoPoly)
        )
    }
}
