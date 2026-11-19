//! A message at MIDI 1.0: the narrowing of a MIDI 2.0 voice message, and the
//! MIDI 1.0 messages a [`Message`] stands for.

use tutti_ump::{Message, Midi1Bytes, Ump, Voice1, Voice2};

use crate::ExtremeValues;

/// Why a MIDI 2.0 channel voice message has no single MIDI 1.0 message
/// ([`narrow`]'s refusal): the kind of message, as M2-104 Appendix D.2 groups
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NoMidi1Form {
    /// A Registered or Assignable Per-Note Controller, or Per-Note Pitch Bend
    /// (D.2.8): MIDI 1.0 has no per-note expression.
    #[error("a per-note controller or bend has no MIDI 1.0 message")]
    PerNote,
    /// A Registered or Assignable Controller, absolute or relative: MIDI 1.0
    /// sends it as four Control Changes ([`Midi1Translator`](crate::Midi1Translator), which
    /// keeps the select).
    #[error("a registered or assignable controller is four MIDI 1.0 messages")]
    Parameter,
    /// Per-Note Management: MIDI 1.0 has no per-note state to manage.
    #[error("per-note management has no MIDI 1.0 message")]
    PerNoteManagement,
}

/// The Default Translation; [`narrow_with`] and
/// [`Midi1Translator::with_mode`](crate::Midi1Translator::with_mode) run the alternate ones.
///
/// `voice` at MIDI 1.0 resolution (M2-115 section 3.4), or the [`NoMidi1Form`]
/// that says why MIDI 1.0 has no single message for it: per-note controllers
/// and bend, RPN and NRPN, per-note management. For a [`Voice2`] you already
/// hold; [`narrow_message`] does the same to a whole [`Message`].
///
/// A Note On whose velocity narrows to 0 becomes velocity 1, because a MIDI 1.0
/// Note On at velocity 0 is a Note Off (M2-104 section 7.4.2). A Program Change
/// keeps its program and loses its bank; [`midi1()`](fn@midi1) has the bank. A reserved
/// attribute type is not read: MIDI 1.0 has no attribute.
///
/// # Errors
///
/// [`NoMidi1Form`] for a per-note message, a registered or assignable
/// controller, or per-note management.
pub fn narrow(voice: Voice2) -> Result<Voice1, NoMidi1Form> {
    narrow_with(ExtremeValues::Scaled, voice)
}

/// [`narrow`] with the extreme values of `extremes` (M2-104 D.1.5): with
/// [`Avoided`](ExtremeValues::Avoided), a value above its minimum narrows to at
/// least 1 and one below its maximum to at most the maximum less 1.
///
/// # Errors
///
/// [`NoMidi1Form`] for a per-note message, a registered or assignable
/// controller, or per-note management, as for [`narrow`].
pub fn narrow_with(extremes: ExtremeValues, voice: Voice2) -> Result<Voice1, NoMidi1Form> {
    Ok(match voice {
        Voice2::NoteOn { note, velocity, .. } => Voice1::NoteOn {
            note,
            velocity: extremes.seven(velocity.to_note_on_u7(), u32::from(velocity.get()), 16),
        },
        Voice2::NoteOff { note, velocity, .. } => Voice1::NoteOff {
            note,
            velocity: extremes.seven(velocity.to_u7(), u32::from(velocity.get()), 16),
        },
        Voice2::PolyPressure { note, value } => Voice1::PolyPressure {
            note,
            pressure: extremes.seven(value.to_u7(), value.get(), 32),
        },
        Voice2::ControlChange { controller, value } => Voice1::ControlChange {
            controller,
            value: extremes.seven(value.to_u7(), value.get(), 32),
        },
        Voice2::ProgramChange { program, .. } => Voice1::ProgramChange(program),
        Voice2::ChannelPressure(value) => {
            Voice1::ChannelPressure(extremes.seven(value.to_u7(), value.get(), 32))
        }
        Voice2::PitchBend(bend) => Voice1::PitchBend(extremes.fourteen(bend.to_u14(), bend.get())),
        Voice2::RegisteredPerNote { .. }
        | Voice2::AssignablePerNote { .. }
        | Voice2::PerNotePitchBend { .. } => return Err(NoMidi1Form::PerNote),
        Voice2::Registered { .. }
        | Voice2::Assignable { .. }
        | Voice2::RelativeRegistered { .. }
        | Voice2::RelativeAssignable { .. } => return Err(NoMidi1Form::Parameter),
        Voice2::PerNoteManagement { .. } => return Err(NoMidi1Form::PerNoteManagement),
    })
}

/// The Default Translation; see [`Midi1Translator::with_mode`](crate::Midi1Translator::with_mode) for the alternates.
///
/// `message` as MIDI 1.0 when it is a MIDI 2.0 channel voice message
/// ([`narrow`], Group and Channel kept); every other message as it came. The
/// inverse of [`widen_message`](crate::widen_message), and the one to reach for
/// when you hold a [`Message`] and want a [`Message`].
///
/// Stateless, like `narrow`: a Program Change keeps its program and loses its
/// bank, and a Registered or Assignable Controller is refused. A stream that
/// must keep them runs a [`Midi1Translator`](crate::Midi1Translator); the wire bytes are
/// [`midi1()`](fn@midi1).
///
/// # Examples
///
/// ```
/// use tutti_midi_protocol::narrow_message;
/// use tutti_ump::{
///     Address, Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, U7, Voice1, Voice2,
/// };
///
/// let to = Address::new(MidiGroup::of(2), MidiChannel::of(5));
/// let on2 = Message::Midi2(to, Voice2::note_on(NoteNumber::of(60), NoteVelocity::from(U7::of(100))));
/// let on1 = narrow_message(on2).unwrap();
/// assert_eq!(on1, Message::midi1(to, Voice1::note_on(NoteNumber::of(60), U7::of(100))));
/// ```
///
/// # Errors
///
/// [`NoMidi1Form`] for a per-note message, a registered or assignable
/// controller, or per-note management, as for [`narrow`].
pub fn narrow_message(message: Message) -> Result<Message, NoMidi1Form> {
    match message {
        Message::Midi2(address, voice) => Ok(Message::Midi1(address, narrow(voice)?)),
        other => Ok(other),
    }
}

/// The Default Translation; see [`Midi1Translator::with_mode`](crate::Midi1Translator::with_mode) for the alternates.
///
/// The MIDI 1.0 messages `message` stands for, in the order to send them:
/// none when it has no MIDI 1.0 form, three for a MIDI 2.0 Program Change with
/// a bank (CC 0 the bank MSB, CC 32 the LSB, then the Program Change; M2-104
/// Appendix D), one otherwise. Channel voice messages narrow as [`narrow`] does.
///
/// The bytes of the wire, for an output that speaks MIDI 1.0 and has no state
/// to keep: a stream that must keep Registered and Assignable Controllers is a
/// [`Midi1Translator`](crate::Midi1Translator). Takes a [`Message`] or the [`Ump`] it is (a
/// packet that does not decode has no MIDI 1.0 form).
pub fn midi1(packet: impl Into<Ump>) -> impl Iterator<Item = Midi1Bytes> {
    let none = None;
    let slots = match packet.into().decode() {
        Ok(message) => slots_of(message),
        Err(_) => [none; 3],
    };
    slots.into_iter().flatten()
}

fn slots_of(message: Message) -> [Option<Midi1Bytes>; 3] {
    let none = None;
    match message {
        Message::Midi1(address, voice) => [Some(voice.bytes(address.channel)), none, none],
        Message::System(_, system) => [Some(system.bytes()), none, none],
        Message::Midi2(
            address,
            Voice2::ProgramChange {
                program,
                bank: Some(bank),
            },
        ) => {
            let [msb, lsb] = bank.bytes(address.channel);
            let program = Voice1::ProgramChange(program).bytes(address.channel);
            [Some(msb), Some(lsb), Some(program)]
        }
        Message::Midi2(address, voice) => [
            narrow(voice).ok().map(|voice| voice.bytes(address.channel)),
            none,
            none,
        ],
        Message::Utility(_)
        | Message::Sysex7(..)
        | Message::Data128(..)
        | Message::Flex(..)
        | Message::Stream(_) => [none; 3],
    }
}
