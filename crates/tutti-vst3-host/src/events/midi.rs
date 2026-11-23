//! A UMP event to VST3 event and back.

use tutti_graph::UmpAt;
use tutti_midi_protocol::PitchBendSensitivity;
use tutti_plugin_types::{
    NoteExpressionType, NoteExpressionValue, WireMidi, expression_of_per_note_controller,
    group_of_port, per_note_message, per_note_tuning, plugin_note_id,
};
use tutti_rt::CappedVec;
use tutti_ump::{
    Address, GroupSpan, Message, MidiChannel, MidiGroup, NoteId, NoteNumber, NoteVelocity, Rpn,
    SysexMessage, Ump, Voice1, Voice2, parse_exact,
};
use tutti_units::{Normalized, Semitones, Velocity};
use tutti_voicing::{LIVE_NOTES, NOTE_CONTROLS, NoteControl, NoteEvent, PerNoteState};

use super::{
    Data, Event, K_DATA_TYPE_MIDI_SYSEX, NoteOff, NoteOn, OutputStream, Payload, PolyPressure,
    ToEvent, expression_type_from_id, sample_offset,
};
use tutti_ump::ControlValue;

/// VST3's tuning expression for `semitones` from the note's key: `0.5 +
/// semitones / 240` (`kTuningTypeID` spans -120 to +120 semitones over
/// `[0, 1]`).
fn tuning_expression(semitones: Semitones) -> f64 {
    0.5 + semitones.get() / TUNING_SPAN
}

/// The semitones `kTuningTypeID` spans, `-120` to `+120`, over `[0, 1]`.
const TUNING_SPAN: f64 = 240.0;

/// The semitones from the note's key a VST3 tuning expression of `value` is:
/// the inverse of [`tuning_expression`], `value` clamped onto `[0, 1]` first.
fn tuning_semitones(value: f64) -> Semitones {
    Semitones::new((value.clamp(0.0, 1.0) - 0.5) * TUNING_SPAN)
}

/// The most events one packet makes: a sensitivity change re-sends a bend for
/// each live note of every key of a channel, a Reset puts five values back for
/// each live note of one key, and a Note On is itself, the values its key held
/// and an attribute.
const MAX_EVENTS: usize = 128 * LIVE_NOTES + 5 * LIVE_NOTES + NOTE_CONTROLS + 16;

/// `kVolumeTypeID`'s normalized value for unity gain: the scale is `0` for
/// silence, `0.25` for 0 dB and `1` for +12 dB (`ivstnoteexpression.h`).
const VOLUME_UNITY: f64 = 0.25;

/// The VST3 volume of a per-note controller's `unit` (`0..=1`, full scale is
/// unity gain): the unity point of VST3's scale, as CLAP's volume does.
fn volume_expression(unit: f64) -> f64 {
    unit * VOLUME_UNITY
}

/// The per-note controller value (`0..=1`, full scale unity) of a plugin's VST3
/// volume `norm`: `4 * norm`, so a boost past 0 dB saturates at full scale
/// rather than rescaling everything below it.
fn volume_unit(norm: f64) -> f64 {
    (norm / VOLUME_UNITY).min(1.0)
}

/// What a Per-Note Management Reset puts each expression of a note back to,
/// as VST3's normalized values: no tuning, unity volume, centred pan and
/// brightness. (Pressure is a `PolyPressure` of 0.)
const EXPRESSION_DEFAULTS: [(NoteExpressionType, f64); 4] = [
    (NoteExpressionType::Tuning, 0.5),
    (NoteExpressionType::Volume, VOLUME_UNITY),
    (NoteExpressionType::Pan, 0.5),
    (NoteExpressionType::Brightness, 0.5),
];

/// Encodes UMP events (`(Offset, Ump)`) as VST3 [`Event`]s, keeping the stream's
/// state across blocks: the per-note state of M2-104 section 7.4
/// ([`PerNoteState`]: each Group, channel and key's held values, the notes that
/// respond to them, each channel's bend sensitivity).
///
/// **A Group is an event bus**: its events carry `busIndex` of the Group, and
/// a Group the plugin has no event input bus for is dropped and counted
/// ([`groups_dropped`](Self::groups_dropped)), never folded onto another
/// Group's channels (M2-104 section 2.1.2).
#[derive(Debug)]
pub struct MidiEncoder {
    notes: PerNoteState,
    /// The plugin's event input buses: a Group is the bus of its number.
    buses: usize,
    groups_dropped: u64,
    unsendable_dropped: u64,
    /// What the last [`encode`](Self::encode) made: room for the most one
    /// packet can make ([`MAX_EVENTS`]), allocated once with the encoder.
    out: CappedVec<Event>,
}

impl Default for MidiEncoder {
    /// An encoder for a plugin with one event input bus.
    fn default() -> Self {
        Self::new(GroupSpan::of(1))
    }
}

impl MidiEncoder {
    /// An encoder for a plugin with `buses` event input buses (1 to 16, the
    /// Groups of MIDI 2.0). A plugin with no event input bus has no encoder:
    /// its [`Stager`](super::Stager) is built
    /// [`without_buses`](super::Stager::without_buses). Every channel's
    /// per-note bend sensitivity starts at the MPE default, ±48 (RP-053).
    /// Allocates (about 270 KiB for each bus): off the audio thread.
    #[must_use]
    pub fn new(buses: GroupSpan) -> Self {
        Self {
            notes: PerNoteState::new(buses, PitchBendSensitivity::MPE_MEMBER_DEFAULT.semitones()),
            buses: usize::from(buses.get()),
            groups_dropped: 0,
            unsendable_dropped: 0,
            out: CappedVec::with_capacity(MAX_EVENTS),
        }
    }

    /// Channel voice messages dropped for a Group the plugin has no event
    /// input bus for, since the encoder was made: each whole (a note, a
    /// controller, a per-note message alike). System Real Time and Utility
    /// messages name no Group's channels and are not counted.
    #[must_use]
    pub fn groups_dropped(&self) -> u64 {
        self.groups_dropped
    }

    /// Messages dropped because VST3 has no event for them, since the encoder
    /// was made: a controller, channel pressure, pitch bend, program change,
    /// RPN or NRPN no `IMidiMapping` parameter took, and a System message. Each
    /// whole. VST3's `DataEvent` has one type, `kMidiSysEx` (ivstevents.h), so
    /// a MIDI 1.0 channel message is never sent in one.
    #[must_use]
    pub fn unsendable_dropped(&self) -> u64 {
        self.unsendable_dropped
    }

    /// The plugin forgot its notes (it was stopped or reset): drops what was
    /// held per note. Never allocates.
    pub fn reset(&mut self) {
        self.notes.clear();
    }

    /// The VST3 events `event` stands for, valid until the next call, at the
    /// stream state the encoder holds.
    ///
    /// The packet is read through `Ump::decode`. A MIDI 1.0 note or key
    /// pressure is translated (`Voice2::from`) and a MIDI 2.0 one is read as is, so
    /// velocity and pressure reach the plugin at their full bit width as `f32`
    /// rather than through 7-bit MIDI 1.
    ///
    /// **Notes and per-note controllers** are the encoder's [`PerNoteState`]:
    /// a Note On is a `NoteOn` with a note id of its own, then the values its
    /// Group, channel and key held before it as expression events (a pressure as
    /// a `PolyPressure`), then its Attribute Pitch; a per-note controller reaches
    /// every note of its key that has not been detached, and a Per-Note
    /// Management Detach or Reset ends or resets them. A Note Off names no id
    /// (VST3's `-1`): the plugin matches it by channel and pitch. A per-note
    /// bend, a Registered Per-Note Controller #3 Pitch and an Attribute Pitch
    /// are one tuning expression in semitones: the note's sounding pitch
    /// ([`PerNoteState`]'s sum of the told pitch and the bend over the
    /// channel's RPN #00/07 sensitivity, consumed: VST3 has no event for it)
    /// relative to the key, sent when it changes; a Reset puts it back to 0 and a
    /// note told nothing has none. A Note On at MIDI 2.0
    /// velocity 0 goes at the smallest velocity above 0, which VST3 plugins do
    /// not read as a release.
    ///
    /// **Every other message with a MIDI 1.0 form** (controllers, channel pitch
    /// bend and pressure, program change, an RPN or NRPN, a System message) has
    /// no VST3 event: `DataEvent` has the one type `kMidiSysEx` (ivstevents.h),
    /// and a plugin reads it as SysEx. A controller reaches a plugin as a
    /// parameter change through `IMidiMapping` (`CcRoute`), before it gets here;
    /// one that no mapping takes is dropped and counted
    /// ([`unsendable_dropped`](Self::unsendable_dropped)). A single-packet SysEx
    /// is a `Data` event.
    ///
    /// The result is empty for a message this host is a sink for, arm by arm in
    /// the encoder: VST3's event list has no raw UMP event, so a message with
    /// no VST3 event and no MIDI 1.0 form, including an unknown packet, goes no
    /// further. A packet of a multi-packet SysEx is empty too: the list
    /// reassembles it. The several events of one packet (a note and its
    /// expression values) are one unit: a caller that cannot stage them all
    /// drops them all.
    pub fn encode(&mut self, event: &UmpAt) -> &[Event] {
        let (at, ump) = event;
        self.out.clear();
        let offset = sample_offset(at.get());
        // A reserved or undefined packet has no VST3 event that carries raw
        // words, so there is no way to forward it: this host is a sink for it.
        if let Ok(message) = (*ump).decode() {
            self.message(offset, message);
        }
        // A packet's events are one unit. `MAX_EVENTS` is the most one makes, so
        // an overflow is a bound that does not hold: the packet is dropped whole
        // and counted, never sent in part.
        if self.out.is_overflowed() {
            self.out.clear();
            self.unsendable_dropped += 1;
        }
        self.out.as_slice()
    }

    fn message(&mut self, offset: i32, message: Message) {
        match message {
            Message::Midi1(address, voice) => match voice {
                // Upscaled here, so a MIDI 1.0 Note On at velocity 0 is the Note
                // Off of M2-104 App. D.3.1, which `voice2` encodes. `voice2`
                // handles a MIDI 2.0 velocity of 0 itself.
                Voice1::NoteOn { .. } | Voice1::NoteOff { .. } | Voice1::PolyPressure { .. } => {
                    self.voice2(offset, address, Voice2::from(voice));
                }
                Voice1::ControlChange { .. }
                | Voice1::ProgramChange(_)
                | Voice1::ChannelPressure(_)
                | Voice1::PitchBend(_) => self.unsendable_dropped += 1,
            },
            Message::Midi2(address, voice) => self.voice2(offset, address, voice),
            Message::System(..) => self.unsendable_dropped += 1,
            // VST3 wants a SysEx with its 0xF0 ... 0xF7 delimiters. A
            // single-packet message fits the inline bytes (at most 6 payload,
            // 8 framed); a packet of a multi-packet message cannot stand
            // alone, and the list reassembles those.
            Message::Sysex7(_, packet) => {
                if let Some(data) = packet
                    .framed()
                    .and_then(|framed| Data::new(K_DATA_TYPE_MIDI_SYSEX, framed))
                {
                    self.out.push(Event::at(offset, Payload::Data(data)));
                }
            }
            // Sinks: VST3 has no event for a clock, a stream message, flex
            // data or SysEx 8, and the Data event carries only MIDI 1.0 bytes.
            Message::Utility(_) | Message::Data128(..) | Message::Flex(..) | Message::Stream(_) => {
            }
        }
    }

    /// The event bus of `address`'s Group, or `None` (counted) when the plugin
    /// has none.
    fn bus_of(&mut self, address: Address) -> Option<i32> {
        let bus = u16::from(address.group.get());
        if usize::from(bus) < self.buses {
            Some(i32::from(bus))
        } else {
            self.groups_dropped += 1;
            None
        }
    }

    fn voice2(&mut self, offset: i32, address: Address, voice: Voice2) {
        let sensitivity = matches!(
            voice,
            Voice2::Registered { bank, index, .. }
                if Rpn::new(bank, index) == Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY
        );
        match voice {
            Voice2::NoteOn { .. }
            | Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::PerNotePitchBend { .. }
            | Voice2::PolyPressure { .. }
            | Voice2::PerNoteManagement { .. } => self.per_note(offset, address, voice),
            Voice2::Registered { .. } if sensitivity => self.per_note(offset, address, voice),
            // The attribute of a Note Off is not read, and its release velocity
            // is kept at its full width. A Note Off names no note: VST3's `-1`,
            // matched by channel and pitch, as MIDI's is.
            Voice2::NoteOff { note, velocity, .. } => {
                if let Some(bus) = self.bus_of(address) {
                    let off = note_off(offset, address.channel, note, velocity.get());
                    self.out.push(on_bus(off, bus));
                }
            }
            // CC, channel pressure, channel pitch bend and program change ride
            // the plugin's parameter funnel (`CcRoute`) when `IMidiMapping`
            // maps them; VST3 has no event for them otherwise (the per-note
            // bend sensitivity is consumed by `per_note`). Dropped, counted.
            Voice2::ControlChange { .. }
            | Voice2::ChannelPressure(_)
            | Voice2::PitchBend(_)
            | Voice2::ProgramChange { .. }
            | Voice2::Registered { .. }
            | Voice2::Assignable { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. } => self.unsendable_dropped += 1,
        }
    }

    /// A message the per-note state reads: a Note On, a per-note controller,
    /// Per-Note Management or the bend sensitivity.
    fn per_note(&mut self, offset: i32, address: Address, voice: Voice2) {
        let Some(bus) = self.bus_of(address) else {
            return;
        };
        let channel = address.channel;
        let (note, velocity) = match voice {
            Voice2::NoteOn { note, velocity, .. } => (Some(note), Some(velocity)),
            Voice2::RegisteredPerNote { note, .. }
            | Voice2::AssignablePerNote { note, .. }
            | Voice2::PerNotePitchBend { note, .. }
            | Voice2::PolyPressure { note, .. }
            | Voice2::PerNoteManagement { note, .. } => (Some(note), None),
            _ => (None, None),
        };
        for event in self.notes.step(address, voice) {
            let made = match *event {
                NoteEvent::Started { id, note, .. } => {
                    // A MIDI 2.0 Note On at velocity 0 is a Note On (M2-104
                    // §7.4.2: velocity 0 "does not function as a Note Off"). VST3
                    // plugins, and the SDK's own MIDI conversion, read a NoteOnEvent
                    // at velocity 0 as a release, so it goes at the smallest
                    // velocity the field carries above 0: M2-104 D.2's rule for a
                    // note-on that cannot be 0 (velocity 0 -> 1). CLAP carries the
                    // 0. A translated MIDI 1.0 Note On at velocity 0 never gets here:
                    // it arrives as a Note Off.
                    let velocity = velocity.map_or(1, |v| v.get().max(1));
                    self.out.push(on_bus(
                        note_on(offset, channel, note, velocity, plugin_note_id(id)),
                        bus,
                    ));
                    continue;
                }
                // A Pitch is the note's sounding pitch, the told pitch with its
                // bend already in it: VST3's one tuning is its distance from the
                // key.
                NoteEvent::Control { id, note, control } => {
                    control_event(offset, channel, note, id, &control)
                }
                NoteEvent::Reset { id } => {
                    for (expression_type, value) in EXPRESSION_DEFAULTS {
                        let reset = NoteExpressionValue {
                            sample_offset: offset,
                            note_id: plugin_note_id(id),
                            expression_type,
                            value,
                        }
                        .to_event();
                        self.out.extend(reset.map(|e| on_bus(e, bus)));
                    }
                    if let Some(key) = note {
                        self.out.push(on_bus(
                            pressure_event(offset, channel, key, id, Normalized::of(0.0)),
                            bus,
                        ));
                    }
                    continue;
                }
            };
            self.out.extend(made.map(|e| on_bus(e, bus)));
        }
    }
}

/// `event` on event bus `bus`.
fn on_bus(mut event: Event, bus: i32) -> Event {
    event.header.bus_index = bus;
    event
}

/// A `PolyPressure` of the note `id` on `note`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "VST3's pressure word is `f32`"
)]
fn pressure_event(
    offset: i32,
    channel: MidiChannel,
    note: NoteNumber,
    id: NoteId,
    pressure: Normalized,
) -> Event {
    Event::at(
        offset,
        Payload::PolyPressure(PolyPressure::new(
            channel.get().into(),
            note.get().into(),
            pressure.get() as f32,
            plugin_note_id(id),
        )),
    )
}

/// The event a per-note controller is for the note `id`, or `None` when VST3
/// has no event for it. A pressure is a `PolyPressure` of `note`, a Pitch is a
/// tuning relative to `note`, and a Registered or Assignable controller with a
/// VST3 expression is that expression.
fn control_event(
    offset: i32,
    channel: MidiChannel,
    note: NoteNumber,
    id: NoteId,
    control: &NoteControl,
) -> Option<Event> {
    let expression = |expression_type, value: f64| {
        let value = if expression_type == NoteExpressionType::Volume {
            volume_expression(value)
        } else {
            value
        };
        NoteExpressionValue {
            sample_offset: offset,
            note_id: plugin_note_id(id),
            expression_type,
            value,
        }
        .to_event()
    };
    match *control {
        NoteControl::Pitch(pitch) => expression(
            NoteExpressionType::Tuning,
            tuning_expression(Semitones::new(pitch.note_number() - f64::from(note.get()))),
        ),
        NoteControl::Pressure(value) => Some(pressure_event(
            offset,
            channel,
            note,
            id,
            value.normalized(),
        )),
        NoteControl::Registered(controller, value) => expression(
            expression_of_per_note_controller(controller.number())?,
            value.normalized().get(),
        ),
        NoteControl::Assignable(index, value) => expression(
            expression_of_per_note_controller(index)?,
            value.normalized().get(),
        ),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "VST3's velocity word is `f32`"
)]
fn note_on(
    offset: i32,
    channel: MidiChannel,
    note: NoteNumber,
    velocity: u16,
    note_id: i32,
) -> Event {
    Event::at(
        offset,
        Payload::NoteOn(NoteOn::new(
            channel.get().into(),
            note.get().into(),
            NoteVelocity::new(velocity).velocity().get() as f32,
            note_id,
        )),
    )
}

/// A note-off naming no note (`-1`).
#[allow(
    clippy::cast_possible_truncation,
    reason = "VST3's velocity word is `f32`"
)]
fn note_off(offset: i32, channel: MidiChannel, note: NoteNumber, velocity: u16) -> Event {
    Event::at(
        offset,
        Payload::NoteOff(NoteOff::new(
            channel.get().into(),
            note.get().into(),
            NoteVelocity::new(velocity).velocity().get() as f32,
            -1,
        )),
    )
}

/// A VST3 `i16` channel field as a 4-bit MIDI channel.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 4 bits, so the narrowing cannot lose a bit the mask keeps"
)]
pub(super) fn midi_channel(raw: i16) -> MidiChannel {
    MidiChannel::masked(raw as u8)
}

/// A VST3 `i16` pitch field as a 7-bit note number.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 7 bits, so the narrowing cannot lose a bit the mask keeps"
)]
fn note_number(raw: i16) -> NoteNumber {
    NoteNumber::masked(raw as u8)
}

/// Group 1 and the VST3 channel field, the address of a decoded event.
fn address(group: MidiGroup, channel: i16) -> Address {
    Address::new(group, midi_channel(channel))
}

/// See [`EventExt::to_midi`](super::EventExt::to_midi).
pub(super) fn decode(event: &Event, stream: &mut OutputStream) -> Option<WireMidi> {
    let frame = event.sample_offset().max(0).cast_unsigned();
    // An output event's `busIndex` is the event output bus it left on (ivstevents.h),
    // and an event bus is a UMP Group, as it is on the input side. A bus past the
    // 16 Groups names none, and the event is dropped.
    let group = group_of_port(u16::try_from(event.header.bus_index).ok()?)?;
    // Notes and poly-pressure build native MIDI-2 Channel Voice events at their
    // full 16 and 32-bit width; VST3's `f32` velocity and pressure words widen
    // exactly into `Velocity` and `Normalized`.
    let built = match &event.payload {
        Payload::NoteOn(e) => {
            stream.note_on(e.noteId, midi_channel(e.channel), note_number(e.pitch));
            Ump::from(Message::Midi2(
                address(group, e.channel),
                Voice2::note_on(
                    note_number(e.pitch),
                    NoteVelocity::from_velocity(Velocity::new_clamped(f64::from(e.velocity))),
                ),
            ))
        }
        Payload::NoteOff(e) => {
            stream.note_off(e.noteId, midi_channel(e.channel), note_number(e.pitch));
            Ump::from(Message::Midi2(
                address(group, e.channel),
                Voice2::note_off(
                    note_number(e.pitch),
                    NoteVelocity::from_velocity(Velocity::new_clamped(f64::from(e.velocity))),
                ),
            ))
        }
        Payload::PolyPressure(e) => Ump::from(Message::Midi2(
            address(group, e.channel),
            Voice2::PolyPressure {
                note: note_number(e.pitch),
                value: ControlValue::from_normalized(Normalized::clamped(f64::from(e.pressure))),
            },
        )),
        Payload::Data(d) => {
            let bytes = d.payload();
            if bytes.len() < 2 {
                stream.count_untranslated();
                return None;
            }
            // The 0xF0 framing is the reliable SysEx signal: the kMidiSysEx subtype is 0,
            // as is the raw channel-voice fallback's. Strip the delimiters and rebuild a
            // single packet; a message too long for one packet is dropped, not truncated,
            // and so is one with a payload byte of 128 or more (malformed); both are
            // counted lost.
            if bytes[0] == 0xF0 {
                let single = SysexMessage::from_framed(bytes)
                    .ok()
                    .map(|message| message.packets(group))
                    .filter(|packets| packets.len() == 1)
                    .and_then(|mut packets| packets.next());
                let Some(ump) = single else {
                    stream.count_sysex_dropped();
                    return None;
                };
                return Some(WireMidi { offset: frame, ump });
            }
            // A MIDI 1.0 message through the output's translator, which gives
            // Channel Voice 2 (M2-104 Appendix D.3). System messages pass
            // through unchanged. Bytes that are no message are counted lost; one
            // the translator takes into a run is not (it is sent when the run ends).
            let Some(message) = parse_exact(group, bytes) else {
                stream.count_untranslated();
                return None;
            };
            return Some(WireMidi {
                offset: frame,
                ump: stream.translator.translate(message)?,
            });
        }
        Payload::LegacyCcOut(e) => {
            return super::legacy_cc::decode(e, frame, group, stream);
        }
        Payload::ExpressionValue(e) => {
            return expression(stream, e.typeId, e.noteId, e.value, frame, group);
        }
        // A reassembled SysEx's bytes are in the list's buffer, which the list drains.
        Payload::SysEx(_) => return None,
        // Dimensions with no per-note or MIDI message of their own (an
        // expression's text or integer value, a chord, a scale): deliberately not
        // translated, and counted lost.
        Payload::ExpressionText(_)
        | Payload::Chord(_)
        | Payload::Scale(_)
        | Payload::ExpressionInt(_) => {
            stream.count_untranslated();
            return None;
        }
    };
    Some(WireMidi {
        offset: frame,
        ump: built,
    })
}

/// The per-note message a plugin's note expression (`typeId`, `value` normalized
/// on `[0, 1]`, tuning `0.5` at the key and `1/240` per semitone) of the note
/// `note_id` is, on `group`. `None`, counted lost on the stream, for a dimension
/// with no per-note message (vibrato, expression, a custom id, an unknown id), a
/// non-finite value, an id no note of the plugin holds, or a tuning past what
/// Pitch 7.25 holds.
fn expression(
    stream: &mut OutputStream,
    type_id: u32,
    note_id: i32,
    value: f64,
    frame: u32,
    group: MidiGroup,
) -> Option<WireMidi> {
    if !value.is_finite() {
        stream.count_expression_dropped();
        return None;
    }
    let Some((channel, note)) = stream.note_of(note_id) else {
        stream.count_expression_dropped();
        return None;
    };
    let voice =
        expression_type_from_id(type_id).and_then(|expression_type| match expression_type {
            NoteExpressionType::Tuning => per_note_tuning(note, tuning_semitones(value)),
            NoteExpressionType::Volume => per_note_message(
                NoteExpressionType::Volume,
                note,
                Normalized::clamped(volume_unit(value)),
            ),
            other => per_note_message(other, note, Normalized::clamped(value)),
        });
    let Some(voice) = voice else {
        stream.count_expression_dropped();
        return None;
    };
    Some(WireMidi {
        offset: frame,
        ump: Message::Midi2(Address { group, channel }, voice).into(),
    })
}
