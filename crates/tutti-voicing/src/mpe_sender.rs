//! MIDI 2.0 as an MPE Sender sends it: the [`MpeSender`] an output edge runs.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cmp::Reverse;

use tutti_midi_protocol::{
    ControllerSupport, Midi2Translator, Mpe11Zone, Mpe11Zones, MpeExpression, MpeFeatures,
    MpeLayout, PitchBendSensitivity, PitchBendSupport, ProfileZone, ProfileZones,
};
use tutti_ump::{
    Address, Attribute, BendValue, CCNumber, ControlValue, GroupSpan, Message, MidiChannel,
    MidiGroup, NoteId, NoteNumber, NoteVelocity, RegisteredData, RegisteredPerNoteController, Rpn,
    U7, Ump, Voice2,
};
use tutti_units::{Bipolar, Semitones};

use crate::{NoteControl, NoteEvent, PerNoteState};

const CHANNELS: usize = MidiChannel::COUNT as usize;
const GROUPS: usize = MidiGroup::COUNT as usize;

/// The per-note controller of MPE's third dimension of control (M2-120 section
/// 4.5), the one a receiver makes of Control Change 74 on a Member.
const THIRD_DIMENSION: u8 = CCNumber::BRIGHTNESS.get();

/// The Control Changes from Omni Off to Poly Mode On.
const OMNI_OFF: u8 = 124;

/// MIDI 2.0 in, MPE out: what an output edge runs for a device that has the
/// MIDI-CI MPE Profile enabled (M2-120), or that an app configured as MPE 1.1
/// (M1-100). The inverse of a receiver's MPE
/// translation, for the MIDI 2.0 and the MIDI 1.0 Protocol alike: its output is
/// MIDI 2.0 Channel Voice, which a [`Midi1Translator`](tutti_midi_protocol::Midi1Translator)
/// makes MIDI 1.0.
///
/// A Group with no zone, and a channel in none, passes unchanged. In a zone
/// ([`set_profile`](Self::set_profile), [`set_mpe11`](Self::set_mpe11)):
///
/// - **A note takes a Member Channel of its own** while one plays none: the one
///   idle longest. With none idle it shares one that does not play its key, if
///   there is one: the one that plays the fewest notes, and of those was given a
///   note longest ago (section 3.6.1 leaves the choice to the sender). Any
///   channel of the zone is a source of notes, the Manager too.
/// - **A Member's Pitch Bend is the whole bend of its note**: the source
///   channel's Pitch Bend over its Pitch Bend Sensitivity (RPN 0/0), the Per-Note
///   Pitch Bend over its (RPN 0/7), and the pitch a Note On attribute or
///   Registered Per-Note Controller 3 gives ([`PerNoteState`] has the rules of
///   those), over the one bend range of the zone (section 3.5). A bend past the
///   range is sent as the range's end. Nothing is sent as the Manager's Pitch
///   Bend, which would move the notes of every source channel.
/// - **A Member's Channel Pressure** is the greater of its note's and its
///   source channel's; its **Control Change 74** is its note's (Assignable
///   Per-Note Controller 74, section 4.5), else its source channel's, else the
///   centre. Each is sent before the Note On (section 5) and as it changes, in
///   the form the zone's receiver declared ([`MpeFeatures`], section 3.7): the
///   bipolar Registered Controller of [`MpeExpression`] when it supports that,
///   never together with the Control Change form (section 4.9), the Control
///   Change form when it supports only that, and not at all when it uses
///   neither. Pitch Bend is sent when the receiver uses it. A source's bipolar
///   Registered Controller is read as the control it stands for.
/// - **The bend range** is the widest Per-Note Pitch Bend Sensitivity of the
///   zone's source channels, at least the range the zone was set with, whole
///   semitones up to 96. A Profile zone is sent it on the Manager (section 3.5);
///   an MPE 1.1 zone on every Member Channel (M1-100 section 2.2.5), and the
///   Manager's own range is never sent.
/// - **Every other message** is sent on the Manager Channel as it is, when the
///   Manager is its source (section 4.1). From a Member nothing is sent: it would
///   act on one source channel of many, and the zone has one Manager. Its All
///   Sound Off and All Notes Off are the Note Offs of its notes.
/// - **Never sent**: Control Changes 124 to 127 (section 4.7) and the per-note
///   controllers MPE has no message for (section 4.11).
///
/// On a Member that plays several notes the last change of any of them is what
/// the channel holds; a note that joins one changes nothing on it.
///
/// For a control thread: it allocates. What was fed and then not sent is undone
/// by a clone made before.
#[derive(Clone, Debug)]
pub struct MpeSender {
    /// Reads a source's MIDI 1.0 messages, whose controllers are runs of several.
    translator: Box<Midi2Translator>,
    /// What each channel of each Group was told, in a zone or not.
    sources: Box<[[Source; CHANNELS]; GROUPS]>,
    groups: [Group; GROUPS],
}

impl Default for MpeSender {
    fn default() -> Self {
        Self::new()
    }
}

impl MpeSender {
    /// A sender with no zone, which passes everything.
    #[must_use]
    pub fn new() -> Self {
        Self {
            translator: Box::default(),
            sources: Box::new([[Source::DEFAULT; CHANNELS]; GROUPS]),
            groups: [const { Group::NONE }; GROUPS],
        }
    }

    /// Takes `zones` as the MPE Profile zones enabled on `group`. The notes of a
    /// zone that went, or whose channels changed, are forgotten, since the
    /// receiver stops them (M2-120 section 3.3); a zone that stays keeps its
    /// notes, whatever its range, response or features. A zone's bend range is
    /// sent before its first note.
    ///
    /// While a Group has Profile zones the MPE 1.1 zones of
    /// [`set_mpe11`](Self::set_mpe11) are held and not used; once the Profile
    /// zones go they apply, and their Configuration Messages are sent ahead of
    /// the next packet fed on the Group.
    pub fn set_profile(&mut self, group: MidiGroup, zones: Option<ProfileZones>) {
        self.groups[group.index()].profile = zones;
        self.apply(group);
    }

    /// Takes `zones` as the MPE 1.1 zones (M1-100) an app chose for `group`, and
    /// calls `out` at once with the MPE Configuration Message of each zone that
    /// starts or changes its Member Channels (section 2.2.1), and with one that
    /// has no Member Channels for each zone that goes. The notes of a zone that
    /// went, or whose channels changed, are forgotten, since the receiver ends
    /// them (section 2.2.3); a zone that stays keeps its notes and is sent no
    /// message. A zone's Member range is sent before its first note, as a
    /// Profile zone's is. While the Group has Profile zones
    /// ([`set_profile`](Self::set_profile)) these zones are held and nothing is
    /// sent.
    pub fn set_mpe11(
        &mut self,
        group: MidiGroup,
        zones: Option<Mpe11Zones>,
        mut out: impl FnMut(Ump),
    ) {
        self.groups[group.index()].mpe11 = zones;
        self.apply(group);
        self.configure(group, &mut out);
    }

    /// Takes the zones `group` runs from the zones it was given.
    fn apply(&mut self, group: MidiGroup) {
        let sources = &self.sources[group.index()];
        let held = &mut self.groups[group.index()];
        match held.zones() {
            None => held.zoned = None,
            Some(zones) => held
                .zoned
                .get_or_insert_with(|| Box::new(Zoned::new(zones)))
                .set(zones, sources),
        }
    }

    /// Calls `out` with the MPE Configuration Messages that take what the
    /// receiver of `group` was configured with to the MPE 1.1 zones in use.
    fn configure(&mut self, group: MidiGroup, out: &mut impl FnMut(Ump)) {
        let held = &mut self.groups[group.index()];
        let wanted = if held.profile.is_some() {
            held.configured
        } else {
            held.mpe11
        };
        Mpe11Zones::configuration(held.configured, wanted, group).for_each(out);
        held.configured = wanted;
    }

    /// The receiver may hold other values than it was told: each Member is told
    /// its own again at its next change or note, and each zone its bend range.
    pub fn resend(&mut self) {
        for zoned in self.groups.iter_mut().filter_map(|g| g.zoned.as_mut()) {
            zoned.resend();
        }
    }

    /// Calls `out` with each message `packet` becomes, in the order to send
    /// them, and returns whether MPE carries it: `false` for a message of a
    /// zone that is never sent, which calls `out` with nothing. One that is
    /// carried may call it with nothing too, when it changes no sounding
    /// note. A packet that is not channel voice, or is of a channel in no
    /// zone, is itself; in a zone a MIDI 1.0 message takes the rules of the
    /// MIDI 2.0 one it means ([`Midi2Translator`]).
    pub fn feed(&mut self, packet: Ump, mut out: impl FnMut(Ump)) -> bool {
        let Ok(Message::Midi1(address, _) | Message::Midi2(address, _)) = packet.decode() else {
            out(packet);
            return true;
        };
        let meant = match self
            .translator
            .translate(packet)
            .map(|meant| meant.decode())
        {
            Some(Ok(Message::Midi2(_, voice))) => Some(voice),
            _ => None,
        };
        self.configure(address.group, &mut out);
        let sources = &mut self.sources[address.group.index()];
        let source = &mut sources[address.channel.index()];
        let meant = meant.map(|voice| MpeExpression::to_control_change(source.absolute(voice)));
        if let Some(voice) = meant {
            source.observe(voice);
        }
        let zoned = self.groups[address.group.index()].zoned.as_mut();
        let Some((zone, zoned)) = zoned.and_then(|z| Some((z.zone_of(address.channel)?, z))) else {
            out(packet);
            return true;
        };
        let mut send = sender(address.group, &mut out);
        meant.is_none_or(|voice| zoned.route(&zone, sources, address.channel, voice, &mut send))
    }
}

/// What one Group was given and runs.
#[derive(Clone, Debug)]
struct Group {
    /// The MPE Profile zones a device enabled, which take precedence.
    profile: Option<ProfileZones>,
    /// The MPE 1.1 zones an app chose.
    mpe11: Option<Mpe11Zones>,
    /// The MPE 1.1 zones the receiver was sent Configuration Messages for.
    configured: Option<Mpe11Zones>,
    /// What plays in the zones in use, if there are any.
    zoned: Option<Box<Zoned>>,
}

impl Group {
    const NONE: Self = Self {
        profile: None,
        mpe11: None,
        configured: None,
        zoned: None,
    };

    /// The zones in use: the Profile's, else the MPE 1.1 ones.
    fn zones(&self) -> Option<Zones> {
        match (self.profile, self.mpe11) {
            (Some(zones), _) => Some(Zones::new(zones.zones().map(Zone::from))),
            (None, Some(zones)) => Some(Zones::new(zones.zones().map(Zone::from))),
            (None, None) => None,
        }
    }
}

/// How a zone was configured, which decides where its bend range goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// By the MPE Profile: the one range of the zone is sent on the Manager
    /// (M2-120 section 3.5).
    Profile,
    /// By an MPE Configuration Message: the range is the Members', sent on
    /// each Member Channel (M1-100 section 2.2.5).
    Mpe11,
}

/// A zone of either kind, as the sender runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Zone {
    kind: Kind,
    layout: MpeLayout,
    /// The bend range the zone starts with.
    range: PitchBendSensitivity,
    /// What its receiver declared.
    features: MpeFeatures,
}

impl From<&ProfileZone> for Zone {
    fn from(zone: &ProfileZone) -> Self {
        Self {
            kind: Kind::Profile,
            layout: zone.layout(),
            range: zone.range(),
            features: zone.features(),
        }
    }
}

impl From<&Mpe11Zone> for Zone {
    /// MPE 1.1 has no Pitch Bend, Channel Pressure or third dimension forms
    /// to declare: the zone has the default features.
    fn from(zone: &Mpe11Zone) -> Self {
        Self {
            kind: Kind::Mpe11,
            layout: zone.layout(),
            range: zone.member_range(),
            features: MpeFeatures::default(),
        }
    }
}

impl Zone {
    /// The Manager Channel (MPE 1.1's Master).
    fn manager(&self) -> MidiChannel {
        self.layout.master_channel()
    }

    /// What a zone keeps its notes by: another range or other features do not
    /// stop them (M2-120 section 3.3, M1-100 section 2.2.3).
    fn channels(&self) -> (Kind, MpeLayout) {
        (self.kind, self.layout)
    }

    /// The channels the zone's bend range is sent on.
    fn range_channels(&self) -> impl Iterator<Item = MidiChannel> {
        let manager = (self.kind == Kind::Profile).then(|| self.manager());
        let members = self
            .layout
            .member_channels()
            .filter(|_| self.kind == Kind::Mpe11);
        manager.into_iter().chain(members)
    }
}

/// The zone each channel of a Group is in.
#[derive(Clone, Copy, Debug)]
struct Zones([Option<Zone>; CHANNELS]);

impl Zones {
    fn new(zones: impl Iterator<Item = Zone>) -> Self {
        let mut by_channel = [None; CHANNELS];
        for zone in zones {
            let layout = zone.layout;
            let channels =
                core::iter::once(layout.master_channel()).chain(layout.member_channels());
            for channel in channels {
                by_channel[channel.index()] = Some(zone);
            }
        }
        Self(by_channel)
    }

    fn of(&self, channel: MidiChannel) -> Option<&Zone> {
        self.0[channel.index()].as_ref()
    }
}

/// Sends a MIDI 2.0 message on a channel of `group` through `out`.
fn sender(group: MidiGroup, out: &mut impl FnMut(Ump)) -> impl FnMut(MidiChannel, Voice2) {
    move |channel, voice| out(Message::midi2(Address { group, channel }, voice).into())
}

/// What a source channel was last told of what its notes take from it.
#[derive(Clone, Copy, Debug)]
struct Source {
    bend: BendValue,
    /// Pitch Bend Sensitivity (RPN 0/0).
    bend_range: PitchBendSensitivity,
    /// Sensitivity of Per-Note Pitch Bend (RPN 0/7).
    note_range: PitchBendSensitivity,
    pressure: ControlValue,
    brightness: Option<ControlValue>,
}

impl Source {
    const DEFAULT: Self = Self {
        bend: BendValue::CENTER,
        bend_range: PitchBendSensitivity::from_semitones(U7::of(2)),
        note_range: PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        pressure: ControlValue::MIN,
        brightness: None,
    };

    /// `voice`, a change of a bend range by a delta as the range it makes
    /// (M2-104 section 7.4.8), which stops at either end.
    fn absolute(&self, voice: Voice2) -> Voice2 {
        let Voice2::RelativeRegistered { bank, index, delta } = voice else {
            return voice;
        };
        let rpn = Rpn::new(bank, index);
        let held = match rpn {
            Rpn::PITCH_BEND_SENSITIVITY => self.bend_range.to_channel_data().to_word(),
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY => self.note_range.to_rpn_bits(),
            _ => return voice,
        };
        Voice2::registered(rpn, ControlValue::new(held.saturating_add_signed(delta)))
    }

    fn observe(&mut self, voice: Voice2) {
        match voice {
            Voice2::PitchBend(value) => self.bend = value,
            Voice2::ChannelPressure(value) => self.pressure = value,
            Voice2::ControlChange {
                controller: CCNumber::BRIGHTNESS,
                value,
            } => self.brightness = Some(value),
            Voice2::ControlChange {
                controller: CCNumber::RESET_ALL,
                ..
            } => {
                self.bend = BendValue::CENTER;
                self.pressure = ControlValue::MIN;
            }
            Voice2::Registered { bank, index, value } => {
                let rpn = Rpn::new(bank, index);
                if rpn == Rpn::PITCH_BEND_SENSITIVITY {
                    let data = RegisteredData::from_word(value.get());
                    self.bend_range = PitchBendSensitivity::from_channel_data(data);
                } else if rpn == Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY {
                    self.note_range = PitchBendSensitivity::from_rpn_bits(value.get());
                }
            }
            _ => {}
        }
    }
}

/// A sounding note of a zone.
#[derive(Clone, Copy, Debug)]
struct Note {
    id: NoteId,
    source: MidiChannel,
    key: NoteNumber,
    /// The Member it plays on.
    member: MidiChannel,
    told: Told,
}

/// What a note was told of itself, apart from its channel.
#[derive(Clone, Copy, Debug)]
struct Told {
    /// Its sounding pitch from its key's.
    pitch: Semitones,
    pressure: ControlValue,
    brightness: Option<ControlValue>,
}

impl Told {
    const DEFAULT: Self = Self {
        pitch: Semitones::new(0.0),
        pressure: ControlValue::MIN,
        brightness: None,
    };
}

impl Note {
    /// The source channel and key, which no other sounding note has.
    fn at(&self) -> (MidiChannel, NoteNumber) {
        (self.source, self.key)
    }

    fn take(&mut self, control: NoteControl) {
        match control {
            NoteControl::Pitch(pitch) => {
                let from_key = pitch.note_number() - f64::from(self.key.get());
                self.told.pitch = Semitones::new(from_key);
            }
            NoteControl::Pressure(value) => self.told.pressure = value,
            NoteControl::Assignable(THIRD_DIMENSION, value)
            | NoteControl::Registered(RegisteredPerNoteController::Brightness, value) => {
                self.told.brightness = Some(value);
            }
            NoteControl::Assignable(..) | NoteControl::Registered(..) => {}
        }
    }

    /// What the Member that plays this note of `source` is told, in a zone of `range`.
    fn expression(&self, source: &Source, range: Range) -> Expression {
        let Told {
            pitch,
            pressure,
            brightness,
        } = self.told;
        let channel = source.bend_range.semitones() * source.bend.bipolar().get();
        let bend = (pitch + channel).get() / range.sensitivity().semitones().get();
        Expression {
            bend: BendValue::from_bipolar(Bipolar::clamped(bend)),
            pressure: pressure.max(source.pressure),
            brightness: brightness
                .or(source.brightness)
                .unwrap_or(ControlValue::CENTER),
        }
    }
}

/// The three values of a note an MPE Member carries.
#[derive(Clone, Copy, Debug)]
struct Expression {
    bend: BendValue,
    pressure: ControlValue,
    brightness: ControlValue,
}

/// A zone's bend range: whole semitones, 1 to 96 (M2-120 section 3.5 has devices
/// support up to 96, and send whole semitones).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Range(u8);

impl Range {
    /// The narrowest range that holds `sensitivity`.
    fn covering(sensitivity: PitchBendSensitivity) -> Self {
        // A `u8`: a sensitivity is at most 128 semitones.
        Self(libm::ceil(sensitivity.semitones().get()).clamp(1.0, 96.0) as u8)
    }

    const fn sensitivity(self) -> PitchBendSensitivity {
        PitchBendSensitivity::from_semitones(U7::masked(self.0))
    }
}

/// One Member Channel: what it plays and what it was last sent.
#[derive(Clone, Copy, Debug)]
struct Member {
    /// How many notes the channel plays.
    notes: u16,
    /// The tick the channel was last given a note: of the least loaded, the
    /// oldest takes the next note.
    started: u32,
    /// The tick the channel was last freed: the longest idle is reused first.
    released: u32,
    /// `None` for what the receiver was not told.
    bend: Option<BendValue>,
    pressure: Option<ControlValue>,
    brightness: Option<ControlValue>,
}

impl Member {
    const IDLE: Self = Self {
        notes: 0,
        started: 0,
        released: 0,
        bend: None,
        pressure: None,
        brightness: None,
    };

    /// Sends what of `expression` the channel does not hold yet, in the forms
    /// `features` says the receiver takes: what it does not use is not sent, and
    /// not held either.
    fn tell(
        &mut self,
        channel: MidiChannel,
        expression: Expression,
        features: MpeFeatures,
        send: &mut impl FnMut(MidiChannel, Voice2),
    ) {
        let Expression {
            bend,
            pressure,
            brightness,
        } = expression;
        if features.pitch_bend == PitchBendSupport::Supported
            && self.bend.replace(bend) != Some(bend)
        {
            send(channel, Voice2::PitchBend(bend));
        }
        let third = (MpeExpression::ThirdDimension, features.third_dimension);
        Self::carry(&mut self.brightness, brightness, third, channel, send);
        let pressed = (MpeExpression::Pressure, features.channel_pressure);
        Self::carry(&mut self.pressure, pressure, pressed, channel, send);
    }

    /// Sends `value` of `expression` in the form `support` names when `held` is
    /// not it already.
    fn carry(
        held: &mut Option<ControlValue>,
        value: ControlValue,
        (expression, support): (MpeExpression, ControllerSupport),
        channel: MidiChannel,
        send: &mut impl FnMut(MidiChannel, Voice2),
    ) {
        let Some(voice) = expression.form(support, value) else {
            return;
        };
        if held.replace(value) != Some(value) {
            send(channel, voice);
        }
    }
}

/// One Group's zones and what plays in them.
#[derive(Clone, Debug)]
struct Zoned {
    zones: Zones,
    /// The bend range each zone's receiver was sent, by Manager Channel.
    ranges: [Option<Range>; CHANNELS],
    members: [Member; CHANNELS],
    notes: Vec<Note>,
    per_note: PerNoteState,
    tick: u32,
}

impl Zoned {
    fn new(zones: Zones) -> Self {
        Self {
            zones,
            ranges: [None; CHANNELS],
            members: [Member::IDLE; CHANNELS],
            notes: Vec::new(),
            per_note: PerNoteState::new(
                GroupSpan::of(1),
                PitchBendSensitivity::MPE_MEMBER_DEFAULT.semitones(),
            ),
            tick: 0,
        }
    }

    /// Takes `zones`, and what the channels were told outside a zone. The notes of a
    /// zone whose channels changed are forgotten.
    fn set(&mut self, zones: Zones, sources: &[Source; CHANNELS]) {
        let had = core::mem::replace(&mut self.zones, zones);
        // A zone is the same while it has its channels: another range or set of
        // features does not stop its notes.
        let channels = |zones: &Zones, channel| zones.of(channel).map(Zone::channels);
        let changed = |channel| channels(&had, channel) != channels(&zones, channel);
        while self.end(|n| changed(n.source)).is_some() {}
        for channel in MidiChannel::all() {
            if changed(channel) {
                self.ranges[channel.index()] = None;
                self.members[channel.index()] = Member::IDLE;
            } else if had.of(channel) != zones.of(channel) {
                // Another range or other forms: the zone's notes are told again in
                // them at its next event.
                self.ranges[channel.index()] = None;
                let member = &mut self.members[channel.index()];
                member.bend = None;
                member.pressure = None;
                member.brightness = None;
            }
            let range = sources[channel.index()].note_range;
            let value = ControlValue::new(range.to_rpn_bits());
            let rpn = Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY;
            self.step(channel, Voice2::registered(rpn, value));
        }
    }

    /// Takes the receiver to hold nothing it was told.
    fn resend(&mut self) {
        self.ranges = [None; CHANNELS];
        for member in &mut self.members {
            member.bend = None;
            member.pressure = None;
            member.brightness = None;
        }
    }

    /// The zone `channel` is in.
    fn zone_of(&self, channel: MidiChannel) -> Option<Zone> {
        self.zones.of(channel).copied()
    }

    /// The bend range of `zone`, which its sources need, sent on the channels
    /// that take it when the receiver has another.
    fn fit_range(
        &mut self,
        zone: &Zone,
        sources: &[Source; CHANNELS],
        send: &mut impl FnMut(MidiChannel, Voice2),
    ) -> Range {
        let range = MidiChannel::all()
            .filter(|c| self.zone_of(*c) == Some(*zone))
            .map(|c| Range::covering(sources[c.index()].note_range))
            .fold(Range::covering(zone.range), Range::max);
        let held = &mut self.ranges[zone.manager().index()];
        if held.replace(range) != Some(range) {
            let data = range.sensitivity().to_channel_data();
            let voice = Voice2::registered(
                Rpn::PITCH_BEND_SENSITIVITY,
                ControlValue::new(data.to_word()),
            );
            for channel in zone.range_channels() {
                send(channel, voice);
            }
        }
        range
    }

    /// Tells the Members of the notes of `zone` that `changed` what they now sound
    /// like, where it is not what they hold.
    fn tell(
        &mut self,
        zone: &Zone,
        sources: &[Source; CHANNELS],
        changed: impl Fn(&Note) -> bool,
        send: &mut impl FnMut(MidiChannel, Voice2),
    ) {
        let zones = self.zones;
        let told = |note: &Note| zones.of(note.source) == Some(zone) && changed(note);
        if !self.notes.iter().any(told) {
            return;
        }
        let held = self.ranges[zone.manager().index()];
        let range = self.fit_range(zone, sources, send);
        // A range the receiver did not hold moves every note of the zone.
        let moved = held != Some(range);
        let told = |note: &Note| told(note) || (moved && zones.of(note.source) == Some(zone));
        for note in &self.notes {
            if told(note) {
                let expression = note.expression(&sources[note.source.index()], range);
                let member = &mut self.members[note.member.index()];
                member.tell(note.member, expression, zone.features, send);
            }
        }
    }

    /// Ends the first note that is `ended` and returns it.
    fn end(&mut self, ended: impl Fn(&Note) -> bool) -> Option<Note> {
        let at = self.notes.iter().position(ended)?;
        let note = self.notes.remove(at);
        self.per_note.end(note.id);
        let member = &mut self.members[note.member.index()];
        member.notes -= 1;
        member.released = self.tick;
        Some(note)
    }

    /// The Member of `layout` a new note of `key` takes.
    fn pick(&self, layout: &MpeLayout, key: NoteNumber) -> MidiChannel {
        let age = |tick: u32| Reverse(self.tick.wrapping_sub(tick));
        let plays = |member| {
            let mut notes = self.notes.iter();
            notes.any(|n| n.member == member && n.key == key)
        };
        layout
            .member_channels()
            .min_by_key(|c| match &self.members[c.index()] {
                idle if idle.notes == 0 => (false, 0, age(idle.released)),
                playing => (plays(*c), playing.notes, age(playing.started)),
            })
            .unwrap_or_else(|| layout.first_member())
    }

    /// Sends what `voice` of `source` becomes, and returns whether MPE carries it.
    fn route(
        &mut self,
        zone: &Zone,
        sources: &[Source; CHANNELS],
        source: MidiChannel,
        voice: Voice2,
        send: &mut impl FnMut(MidiChannel, Voice2),
    ) -> bool {
        let manager = source == zone.manager();
        match voice {
            Voice2::NoteOn {
                note: key,
                velocity,
                attribute,
            } => {
                self.fit_range(zone, sources, send);
                let member = self.start(zone, sources, (source, key), voice, send);
                let voice = Voice2::NoteOn {
                    note: key,
                    velocity,
                    // The Member's bend carries the pitch.
                    attribute: match attribute {
                        Attribute::Pitch(_) => Attribute::None,
                        other => other,
                    },
                };
                send(member, voice);
            }
            Voice2::NoteOff { note: key, .. } => {
                if let Some(ended) = self.end(|n| n.at() == (source, key)) {
                    send(ended.member, voice);
                }
            }
            Voice2::AssignablePerNote { controller, .. } if controller != THIRD_DIMENSION => {
                return false;
            }
            Voice2::RegisteredPerNote { controller, .. }
                if !matches!(
                    controller,
                    RegisteredPerNoteController::Brightness | RegisteredPerNoteController::Pitch
                ) =>
            {
                return false;
            }
            Voice2::PerNotePitchBend { note: key, .. }
            | Voice2::PolyPressure { note: key, .. }
            | Voice2::AssignablePerNote { note: key, .. }
            | Voice2::RegisteredPerNote { note: key, .. }
            | Voice2::PerNoteManagement { note: key, .. } => {
                self.step(source, voice);
                self.tell(zone, sources, |n| n.at() == (source, key), send);
            }
            Voice2::PitchBend(_) | Voice2::ChannelPressure(_) => {
                self.tell(zone, sources, |n| n.source == source, send);
            }
            Voice2::Registered { bank, index, .. }
                if [
                    Rpn::PITCH_BEND_SENSITIVITY,
                    Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
                ]
                .contains(&Rpn::new(bank, index)) =>
            {
                self.step(source, voice);
                // Its range may have changed, for every note.
                self.tell(zone, sources, |_| true, send);
            }
            Voice2::ControlChange { controller, .. } => match controller {
                CCNumber::BRIGHTNESS => self.tell(zone, sources, |n| n.source == source, send),
                CCNumber::RESET_ALL => self.reset_controllers(zone, sources, source, voice, send),
                CCNumber::ALL_SOUND_OFF | CCNumber::ALL_NOTES_OFF if manager => {
                    // The receiver ends the notes of the whole zone.
                    send(source, voice);
                    let zones = self.zones;
                    while self.end(|n| zones.of(n.source) == Some(zone)).is_some() {}
                }
                CCNumber::ALL_SOUND_OFF | CCNumber::ALL_NOTES_OFF => {
                    while let Some(note) = self.end(|n| n.source == source) {
                        send(note.member, off(note.key));
                    }
                }
                // Omni Off, Omni On, Mono Mode On and Poly Mode On are sent on no
                // MPE Channel (M2-120 section 4.7).
                c if c.get() >= OMNI_OFF || !manager => return false,
                _ => send(source, voice),
            },
            _ if manager => send(source, voice),
            _ => return false,
        }
        true
    }

    /// Starts the note of the Note On `voice` of `at`, with what its Member needs
    /// sent ahead of it, and returns the Member.
    fn start(
        &mut self,
        zone: &Zone,
        sources: &[Source; CHANNELS],
        at: (MidiChannel, NoteNumber),
        voice: Voice2,
        send: &mut impl FnMut(MidiChannel, Voice2),
    ) -> MidiChannel {
        self.tick = self.tick.wrapping_add(1);
        let (source, key) = at;
        // The same key struck again ends its earlier note, and stays on its
        // Member.
        let again = self.end(|n| n.at() == at);
        if let Some(note) = again {
            send(note.member, off(key));
        }
        let member = again.map_or_else(|| self.pick(&zone.layout, key), |note| note.member);
        for event in self.per_note.step(Address::first(source), voice) {
            match *event {
                NoteEvent::Started { id, .. } => self.notes.push(Note {
                    id,
                    source,
                    key,
                    member,
                    told: Told::DEFAULT,
                }),
                event => take(&mut self.notes, event),
            }
        }
        let slot = &mut self.members[member.index()];
        let first = slot.notes == 0;
        slot.notes += 1;
        slot.started = self.tick;
        // On a channel that plays a note the values are that note's too.
        if first {
            self.tell(zone, sources, |n| n.at() == at, send);
        }
        member
    }

    /// Reads a per-note message of `source`.
    fn step(&mut self, source: MidiChannel, voice: Voice2) {
        for event in self.per_note.step(Address::first(source), voice) {
            take(&mut self.notes, *event);
        }
    }

    /// Reset All Controllers (RP-015) of `source`, which [`Source`] has read.
    fn reset_controllers(
        &mut self,
        zone: &Zone,
        sources: &[Source; CHANNELS],
        source: MidiChannel,
        voice: Voice2,
        send: &mut impl FnMut(MidiChannel, Voice2),
    ) {
        for event in self.per_note.reset_pressure(Address::first(source)) {
            take(&mut self.notes, *event);
        }
        if source == zone.manager() {
            send(source, voice);
            // The receiver reset the bend and pressure of the whole zone.
            for member in zone.layout.member_channels() {
                let member = &mut self.members[member.index()];
                member.bend = Some(BendValue::CENTER);
                member.pressure = Some(ControlValue::MIN);
            }
            self.tell(zone, sources, |_| true, send);
        } else {
            self.tell(zone, sources, |n| n.source == source, send);
        }
    }
}

/// What `event` tells the note it names.
fn take(notes: &mut [Note], event: NoteEvent) {
    let (NoteEvent::Started { id, .. } | NoteEvent::Control { id, .. } | NoteEvent::Reset { id }) =
        event;
    let Some(note) = notes.iter_mut().find(|n| n.id == id) else {
        return;
    };
    match event {
        NoteEvent::Started { .. } => {}
        NoteEvent::Control { control, .. } => note.take(control),
        NoteEvent::Reset { .. } => note.told = Told::DEFAULT,
    }
}

/// The Note Off of `key` at no velocity.
fn off(key: NoteNumber) -> Voice2 {
    Voice2::note_off(key, NoteVelocity::new(0))
}
