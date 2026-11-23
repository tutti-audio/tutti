//! The stateless translation between MIDI 1.0 and MIDI 2.0 channel voice
//! messages (M2-104 Appendix D, M2-115 section 3): `From` and `TryFrom` on the
//! voice types, and the methods that apply them to a [`Message`] or a packet.

use crate::{
    Attribute, BendValue, ControlValue, Message, MidiGroup, NoteVelocity, U7, U14, Ump, Voice1,
    Voice2, parse_exact, scale,
};

/// The release velocity a MIDI 1.0 Note On at velocity 0 becomes (M2-104
/// Appendix D.3.1).
const NOTE_ON_ZERO_RELEASE: NoteVelocity = NoteVelocity::new(0x8000);

/// Why a MIDI 2.0 channel voice message has no single MIDI 1.0 message
/// (the refusal of `Voice1::try_from`): the kind of message, as M2-104
/// Appendix D.2 groups them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NoMidi1Form {
    /// A Registered or Assignable Per-Note Controller, or Per-Note Pitch Bend
    /// (D.2.8): MIDI 1.0 has no per-note expression.
    #[error("a per-note controller or bend has no MIDI 1.0 message")]
    PerNote,
    /// A Registered or Assignable Controller, absolute or relative: MIDI 1.0
    /// sends it as four Control Changes, and a stateful translation keeps the
    /// select.
    #[error("a registered or assignable controller is four MIDI 1.0 messages")]
    Parameter,
    /// Per-Note Management: MIDI 1.0 has no per-note state to manage.
    #[error("per-note management has no MIDI 1.0 message")]
    PerNoteManagement,
}

/// How a value that translates to the smallest or largest MIDI 1.0 value is
/// translated (M2-104 D.1.5): some devices give those two values a meaning of
/// their own, and a translator that knows of it may keep a value that is not
/// the extreme away from them.
///
/// Translating to MIDI 2.0 needs no such mode: a MIDI 1.0 value above 0 is
/// above 0 and one below the maximum is below it (M2-115 section 3.1), so
/// that direction never lands a value on an extreme it was not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ExtremeValues {
    /// The Default Translation: the plain shift of M2-115 section 3.4, so a
    /// value just above 0 translates to 0 and one just below the maximum to the
    /// maximum. A Note On velocity is the one exception, and an exception in
    /// every mode: it never translates to 0 (D.2.1).
    #[default]
    Scaled,
    /// A value above its minimum translates to at least 1, and one below its
    /// maximum to at most the maximum less 1. The minimum and the maximum
    /// translate to themselves. It covers every Min-Center-Max scaled value: a
    /// velocity, a pressure, a Control Change, a Pitch Bend and the data of a
    /// Registered or Assignable Controller that is scaled that way. D.1.5 says
    /// "some properties" and names none; this reads the Registered Controllers
    /// that M2-115 section 4.1 zero-extends (indices 0 to 31) as not among
    /// them, since they are fixed-point numbers or pairs of fields with no
    /// special extreme, and they translate as in [`Scaled`](Self::Scaled).
    Avoided,
}

impl ExtremeValues {
    /// `downscaled`, the 7-bit downscaling of `source` (`source_bits` wide), with
    /// this mode's extremes applied.
    #[must_use]
    pub fn seven(self, downscaled: U7, source: u32, source_bits: u32) -> U7 {
        match self {
            Self::Scaled => downscaled,
            Self::Avoided => {
                U7::masked(avoid(u32::from(downscaled.get()), source, source_bits, 7) as u8)
            }
        }
    }

    /// The 14-bit MIDI 1.0 data of Registered Controller `index`, the inverse of
    /// [`ControlValue::from_registered`]: rounded for an index of 0 to 31 (M2-115
    /// section 4.4.1), otherwise [`assignable`](Self::assignable).
    #[inline]
    #[must_use]
    pub fn registered(self, index: U7, value: ControlValue) -> U14 {
        if scale::zero_extended(index) {
            // `rounded_u14` clamps to 14 bits.
            U14::masked(scale::rounded_u14(value.get()))
        } else {
            self.assignable(value)
        }
    }

    /// The 14-bit MIDI 1.0 data of an Assignable Controller, or of a Registered
    /// one that M2-115 scales Min-Center-Max: the top 14 bits (as
    /// [`BendValue::to_u14`] does a bend), with this mode's extremes applied.
    #[inline]
    #[must_use]
    pub fn assignable(self, value: ControlValue) -> U14 {
        self.fourteen(BendValue::new(value.get()).to_u14(), value.get())
    }

    /// `downscaled`, the 14-bit downscaling of the 32-bit `source`, with this
    /// mode's extremes applied.
    #[must_use]
    pub fn fourteen(self, downscaled: U14, source: u32) -> U14 {
        match self {
            Self::Scaled => downscaled,
            Self::Avoided => U14::masked(avoid(u32::from(downscaled.get()), source, 32, 14) as u16),
        }
    }
}

/// `downscaled`, the `dst_bits` wide downscaling of `source` (`source_bits` wide),
/// moved off the minimum when `source` is not 0 and off the maximum when
/// `source` is not its maximum (D.1.5).
fn avoid(downscaled: u32, source: u32, source_bits: u32, dst_bits: u32) -> u32 {
    let source_max = u64::MAX >> (64 - source_bits);
    let dst_max = (1u32 << dst_bits) - 1;
    if downscaled == 0 && source != 0 {
        1
    } else if downscaled == dst_max && u64::from(source) != source_max {
        dst_max - 1
    } else {
        downscaled
    }
}

/// The MIDI 2.0 message that means what the MIDI 1.0 `voice` means (M2-104
/// Appendix D.3, M2-115 section 3). The caller keeps the [`Address`](crate::Address) unchanged.
///
/// Control Changes that would need RPN or Bank Select assembly convert by value
/// as plain Control Changes, and a Program Change has no bank.
impl From<Voice1> for Voice2 {
    #[inline]
    fn from(voice: Voice1) -> Self {
        match voice {
            Voice1::NoteOn { note, velocity } if velocity.get() == 0 => Self::NoteOff {
                note,
                velocity: NOTE_ON_ZERO_RELEASE,
                attribute: Attribute::None,
            },
            Voice1::NoteOn { note, velocity } => Self::NoteOn {
                note,
                velocity: NoteVelocity::from(velocity),
                attribute: Attribute::None,
            },
            Voice1::NoteOff { note, velocity } => Self::NoteOff {
                note,
                velocity: NoteVelocity::from(velocity),
                attribute: Attribute::None,
            },
            Voice1::PolyPressure { note, pressure } => Self::PolyPressure {
                note,
                value: ControlValue::from(pressure),
            },
            Voice1::ControlChange { controller, value } => Self::ControlChange {
                controller,
                value: ControlValue::from(value),
            },
            Voice1::ProgramChange(program) => Self::ProgramChange {
                program,
                bank: None,
            },
            Voice1::ChannelPressure(pressure) => {
                Self::ChannelPressure(ControlValue::from(pressure))
            }
            Voice1::PitchBend(bend) => Self::PitchBend(BendValue::from(bend)),
        }
    }
}

/// The Default Translation: `voice` at MIDI 1.0 resolution (M2-115 section
/// 3.4), or the [`NoMidi1Form`] that says why MIDI 1.0 has no single message
/// for it: per-note controllers and bend, RPN and NRPN, per-note management.
/// For a [`Voice2`] you already hold; [`Message::to_midi1`] does the same
/// to a whole [`Message`] and [`Voice1::from_voice2`] runs the alternate
/// extreme values.
///
/// A Note On whose velocity translates to 0 becomes velocity 1, because a MIDI
/// 1.0 Note On at velocity 0 is a Note Off (M2-104 section 7.4.2). A Program
/// Change keeps its program and loses its bank. A reserved attribute type is not
/// read: MIDI 1.0 has no attribute.
impl TryFrom<Voice2> for Voice1 {
    type Error = NoMidi1Form;

    fn try_from(voice: Voice2) -> Result<Self, NoMidi1Form> {
        Self::from_voice2(voice, ExtremeValues::Scaled)
    }
}

impl Voice1 {
    /// `voice` at MIDI 1.0 resolution with the extreme values of `extremes`
    /// (M2-104 D.1.5): with [`Avoided`](ExtremeValues::Avoided), a value above
    /// its minimum translates to at least 1 and one below its maximum to at most
    /// the maximum less 1.
    ///
    /// # Errors
    ///
    /// [`NoMidi1Form`] for a per-note message, a registered or assignable
    /// controller, or per-note management, as for `Voice1::try_from`.
    pub fn from_voice2(voice: Voice2, extremes: ExtremeValues) -> Result<Self, NoMidi1Form> {
        Ok(match voice {
            Voice2::NoteOn { note, velocity, .. } => Self::NoteOn {
                note,
                velocity: extremes.seven(velocity.to_note_on_u7(), u32::from(velocity.get()), 16),
            },
            Voice2::NoteOff { note, velocity, .. } => Self::NoteOff {
                note,
                velocity: extremes.seven(velocity.to_u7(), u32::from(velocity.get()), 16),
            },
            Voice2::PolyPressure { note, value } => Self::PolyPressure {
                note,
                pressure: extremes.seven(value.to_u7(), value.get(), 32),
            },
            Voice2::ControlChange { controller, value } => Self::ControlChange {
                controller,
                value: extremes.seven(value.to_u7(), value.get(), 32),
            },
            Voice2::ProgramChange { program, .. } => Self::ProgramChange(program),
            Voice2::ChannelPressure(value) => {
                Self::ChannelPressure(extremes.seven(value.to_u7(), value.get(), 32))
            }
            Voice2::PitchBend(bend) => {
                Self::PitchBend(extremes.fourteen(bend.to_u14(), bend.get()))
            }
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
}

impl Message {
    /// This message as MIDI 2.0 when it is a MIDI 1.0 channel voice message
    /// (`Voice2::from`, address kept); every other message as it came.
    ///
    /// Stateless: a MIDI 1.0 RPN run becomes plain Control Changes. An edge
    /// that can see a whole stream runs a MIDI 2.0 translator instead.
    #[inline]
    #[must_use]
    pub fn to_midi2(self) -> Self {
        match self {
            Self::Midi1(address, voice) => Self::Midi2(address, voice.into()),
            other => other,
        }
    }

    /// This message as MIDI 1.0 when it is a MIDI 2.0 channel voice message
    /// (`Voice1::try_from`, Group and Channel kept); every other message as it
    /// came. The inverse of [`to_midi2`](Self::to_midi2), and the one to reach
    /// for when you hold a [`Message`] and want a [`Message`].
    ///
    /// Stateless: a Program Change keeps its program and loses its bank, and a
    /// Registered or Assignable Controller is refused. A stream that must keep
    /// them runs a MIDI 1.0 translator.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_ump::{
    ///     Address, Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, U7, Voice1, Voice2,
    /// };
    ///
    /// let to = Address::new(MidiGroup::of(2), MidiChannel::of(5));
    /// let velocity = NoteVelocity::from(U7::of(100));
    /// let on2 = Message::Midi2(to, Voice2::note_on(NoteNumber::of(60), velocity));
    /// let on1 = on2.to_midi1().unwrap();
    /// assert_eq!(on1, Message::midi1(to, Voice1::note_on(NoteNumber::of(60), U7::of(100))));
    /// ```
    ///
    /// # Errors
    ///
    /// [`NoMidi1Form`] for a per-note message, a registered or assignable
    /// controller, or per-note management.
    pub fn to_midi1(self) -> Result<Self, NoMidi1Form> {
        match self {
            Self::Midi2(address, voice) => Ok(Self::Midi1(address, Voice1::try_from(voice)?)),
            other => Ok(other),
        }
    }
}

impl Ump {
    /// The packet for the MIDI 1.0 message at the start of `bytes` (a plugin's
    /// or a file's wire bytes, padding after the message ignored), in `group`:
    /// read by [`parse_exact`] and made MIDI 2.0 by [`Message::to_midi2`], so a
    /// channel voice message is MIDI 2.0 and a System one is as it is.
    ///
    /// `None` when `parse_exact` reads none: a SysEx or undefined status, a
    /// message cut short, or a data byte with its top bit set.
    #[inline]
    #[must_use]
    pub fn from_midi1_bytes(group: MidiGroup, bytes: &[u8]) -> Option<Self> {
        parse_exact(group, bytes).map(|message| Self::from(message.to_midi2()))
    }
}
