//! Following an outside tempo (feature `midi`): the session's transport takes the tempo and the
//! meter a MIDI 2.0 source sends as Flex Data Set Tempo and Set Time Signature
//! (M2-104 §7.5.3, §7.5.4).
//!
//! The pure core is `tutti_midi_runtime::TempoFollower`: it decodes and says
//! *what* to apply. This module says *where* and *how*: it drains an event tap
//! on the followed Group's channelless port of the input node in
//! [`Phase::Poll`](crate::Phase::Poll), places each change at the next block
//! start, builds the new [`TempoMap`] and [`MeterMap`](tutti_music::MeterMap)
//! and publishes them.
//!
//! **A follow mode lays its steps over the app's maps and never replaces them.**
//! The app's tempo map and meter are the base ([`Bases`]); the map the engine
//! uses (the effective one) is the base up to the beat of the first change,
//! then the live steps. Each live step truncates what came after it: a base
//! point or ramp ahead of the playhead is not played while following, because
//! two authorities for the same future beats is the bug to avoid. A ramp a step
//! lands in is cut at that beat. The steps are kept, so a write to the base
//! while following publishes the new base up to the first step and the steps
//! after it. [`Midi::unfollow_tempo`](crate::Midi::unfollow_tempo) publishes the
//! base as it stands then; the clock re-seats at its current beat, so the
//! playhead does not jump.
//!
//! **Where a change lands.** A change heard in a block is drained in the
//! `Poll` after it and takes effect at the start of the next block, never
//! behind the playhead: placing it at the frame it arrived on would be
//! retroactive. The step's beat is the transport's next block start, a frame
//! (the playhead's transport time plus the call's length) read through the map
//! in force (`TempoView::beat_at_frame`), never `Playhead::beat` extrapolated at
//! a tempo. A publish that lands after the audio thread has rendered past that
//! beat (a slow poll, a big block) moves no beat: the clock re-seats at its
//! current beat on a new map (`tempo_for_block`), so the beat stays continuous
//! and the new tempo applies from the block that reads the map. The step is
//! then behind the playhead in the map, and a bounce made from it renders the
//! tempo from the step's beat as the map says.
//!
//! **The meter** lands at the next bar line at or after that beat (a meter
//! change always begins a bar; M2-104 §7.5.4), over the same base.
//!
//! # Following a MIDI clock
//!
//! [`Midi::follow_clock`](crate::Midi::follow_clock) follows MIDI Beat Clock or
//! MIDI Time Code from one Group the same way: a tap on its channelless port,
//! drained in `Phase::Poll`, read by the pure
//! `tutti_midi_runtime::BeatClockFollower` or `MtcFollower` with each event's arrival on the graph's
//! frame clock: seconds since graph frame 0 (the tap's frame over its rate). A tempo it fits is a
//! step laid over the base, as a Flex Set Tempo's is. Play, stop and locate go to the
//! transport as commands at `At::Frame`.
//!
//! **A command is placed on the frame its message arrived on.** The tap stamps
//! each message with its frame; the command takes effect at that frame plus
//! `CLOCK_LEAD` (four declicks, 20 ms), and never sooner than one declick after
//! the first frame of the next block, which is where the engine sees a command
//! with its whole fade ahead (`placed_frame`). It is `At::Frame` and not
//! `At::NextBlock`, so a poll that is later than the lead lands it on the same
//! frame a prompt one would, one declick past the block. A tempo is placed on
//! the beat the transport reaches at that same frame (`Stand::beat_at`), through
//! the map in force. While the source is rolling it moves on by the time between
//! the arrival and the placed frame, so a locate's position is advanced by it:
//! seconds for MTC, and for Beat Clock beats at the tempo in force. A locate
//! while the source is stopped is not advanced. The position is off only by the
//! time a command is late to the audio thread, which the declick past the block
//! is there to prevent. The Machine Control follower carries no arrival and
//! places at `At::NextBlock`.
//!
//! **Drift.** Once per poll, while the transport rolls, the follower is given
//! where the transport is on the first frame of the next block
//! (`BeatClockFollower::steer`, `MtcFollower::steer`): a beat for Beat Clock, a time for MTC. For Beat
//! Clock the error against the source's beat is closed by a trim of the tempo
//! laid over the base as a step; for either kind an error past
//! `BeatClockFollower::SLIP` or `MtcFollower::SLIP_FRAMES`
//! locates to the source and is reported ([`MidiEvent::ClockSlipped`]). The
//! latest error is [`Midi::clock_error`](crate::Midi::clock_error).
//!
//! **MTC running backward** is reported ([`MidiEvent::ClockReversed`]) and the
//! transport is stopped: it has no negative rate, so a reverse source cannot be
//! followed, and the session takes no position from it until its Quarter Frames
//! ascend again. **A source off its nominal speed** is followed by locates alone
//! (no rate control exists on the transport); the bound is in
//! `tutti_midi_runtime::MtcFollower`'s rules.
//!
//! MTC `00:00:00:00` is beat 0 of the project's tempo map: a position in time
//! is a beat through the map in force.

use std::sync::Arc;

use tutti_engine::Playhead;
use tutti_graph::EventTapReader;
use tutti_midi_protocol::SysexDeviceId;
use tutti_midi_runtime::{
    BeatClockEvent, BeatClockFollower, Followed, MachineControlEvent, MachineControlFollower,
    MtcEvent, MtcFollower, TempoFollower,
};
use tutti_tempo::TempoMap;
use tutti_timeline::{ProjectMeter, TransportSettings};
use tutti_ump::{MidiGroup, Ump};
use tutti_units::{At, Beat, Bpm, Frame, Seconds};

use crate::MidiEvent;
use crate::value::{Bases, Steps};

/// How many events the follower's tap holds between two polls. Set Tempo and
/// Set Time Signature are rare; a source that floods them loses the oldest
/// beyond this, counted and logged.
pub(crate) const FOLLOW_TAP_CAPACITY: tutti_graph::EventCapacity =
    tutti_graph::EventCapacity::of(256);

/// What [`Midi::follow_tempo`](crate::Midi::follow_tempo) follows.
///
/// The source is a Group's Flex Data Set Tempo (and Set Time Signature) on an
/// input port's input node (`MidiInput::node`).
/// MIDI Beat Clock and MIDI Time Code are followed with
/// [`Midi::follow_clock`](crate::Midi::follow_clock) and [`FollowClock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowTempo {
    input: tutti_graph::NodeKey,
    group: MidiGroup,
    meter: bool,
}

impl FollowTempo {
    /// Follows Set Tempo on `group` of the input port whose input node is
    /// `input` (`MidiInput::node`). A Group is a cable of its own (M2-104
    /// §2.1.2, Appendix B.1); the Group must be one the input hears
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    #[must_use]
    pub const fn new(input: tutti_graph::NodeKey, group: MidiGroup) -> Self {
        Self {
            input,
            group,
            meter: false,
        }
    }

    /// Follows Set Time Signature too.
    #[must_use]
    pub const fn meter(mut self) -> Self {
        self.meter = true;
        self
    }

    /// The input node followed.
    pub(crate) const fn input(&self) -> tutti_graph::NodeKey {
        self.input
    }

    /// The Group followed.
    pub(crate) const fn group(&self) -> MidiGroup {
        self.group
    }

    /// Whether Set Time Signature is followed.
    pub(crate) const fn follows_meter(&self) -> bool {
        self.meter
    }
}

/// How many events a clock follower's tap holds between two polls: Timing
/// Clocks come at up to 120 a second (300 bpm) and Quarter Frames at 120, so a
/// poll interval of several seconds is held.
pub(crate) const CLOCK_TAP_CAPACITY: tutti_graph::EventCapacity =
    tutti_graph::EventCapacity::of(1024);

/// Which outside clock [`Midi::follow_clock`](crate::Midi::follow_clock) reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClockKind {
    /// MIDI Beat Clock: Timing Clock, Start, Continue, Stop and Song Position
    /// Pointer. A musical position and a tempo.
    BeatClock,
    /// MIDI Time Code: Quarter Frames and the Full Message. A position in
    /// time, and no tempo.
    Mtc,
}

/// How far the transport is from the clock the session follows, in the unit
/// that clock counts in: the one reading [`Midi::clock_error`](crate::Midi::clock_error)
/// and [`MidiEvent::ClockSlipped`] give for either [`ClockKind`], which the app
/// picks at run time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockOffset {
    /// Beat Clock: the source's beat less the transport's.
    Beat(Beat),
    /// MTC: the source's time less the transport's.
    Time(Seconds),
}

/// What [`Midi::follow_clock`](crate::Midi::follow_clock) follows.
///
/// One Group of an input port
/// (`MidiInput::node`) is read for one kind of
/// clock. A device that sends both Beat Clock and MTC is followed for one: the
/// session has one transport and one authority for its position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowClock {
    input: tutti_graph::NodeKey,
    group: MidiGroup,
    kind: ClockKind,
}

impl FollowClock {
    /// Follows the `kind` of clock that `group` carries on the input port whose
    /// input node is `input` (`MidiInput::node`). A Group is a cable of its own
    /// (M2-104 §2.1.2, Appendix B.1); the Group must be one the input hears
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    #[must_use]
    pub const fn new(input: tutti_graph::NodeKey, group: MidiGroup, kind: ClockKind) -> Self {
        Self { input, group, kind }
    }

    /// The input node followed.
    pub(crate) const fn input(&self) -> tutti_graph::NodeKey {
        self.input
    }

    /// The Group followed.
    pub(crate) const fn group(&self) -> MidiGroup {
        self.group
    }

    /// Which clock it sends.
    pub(crate) const fn kind(&self) -> ClockKind {
        self.kind
    }
}

/// How many events a Machine Control follower's tap holds between two polls:
/// commands are a handful of SysEx packets each, and a controller sends them by
/// hand.
pub(crate) const MACHINE_TAP_CAPACITY: tutti_graph::EventCapacity =
    tutti_graph::EventCapacity::of(256);

/// What [`Midi::follow_machine_control`](crate::Midi::follow_machine_control)
/// follows: MIDI Machine Control commands (RP-013) on one Group of an input port
/// (`MidiInput::node`).
///
/// MMC carries no clock and no tempo, so it follows beside a
/// [`FollowClock`] and never contends with it for the tempo map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowMachineControl {
    input: tutti_graph::NodeKey,
    group: MidiGroup,
    device: SysexDeviceId,
}

impl FollowMachineControl {
    /// Follows the commands `group` of the input port whose input node is
    /// `input` (`MidiInput::node`) sends to `device`, this session's MMC device
    /// ID: a command to it, to a group it was assigned to, or to the all-call
    /// (`7F`, always accepted) is followed. A Group is a cable of its own
    /// (M2-104 §2.1.2, Appendix B.1); the Group must be one the input hears
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    #[must_use]
    pub const fn new(input: tutti_graph::NodeKey, group: MidiGroup, device: SysexDeviceId) -> Self {
        Self {
            input,
            group,
            device,
        }
    }

    /// The input node followed.
    pub(crate) const fn input(&self) -> tutti_graph::NodeKey {
        self.input
    }

    /// The Group followed.
    pub(crate) const fn group(&self) -> MidiGroup {
        self.group
    }

    /// This session's MMC device ID.
    pub(crate) const fn device(&self) -> SysexDeviceId {
        self.device
    }
}

/// The one tempo follower a session has: its core and its tap.
pub(crate) struct Follow {
    core: TempoFollower,
    tap: EventTapReader,
}

/// What a `Poll` phase reads besides the follower.
pub(crate) struct FollowCx<'a> {
    pub(crate) settings: &'a TransportSettings,
    pub(crate) meter: &'a ProjectMeter,
    /// The base the live steps are laid over.
    pub(crate) bases: &'a Bases,
    pub(crate) playhead: Playhead,
}

impl Follow {
    /// A follower reading `tap`.
    pub(crate) fn new(core: TempoFollower, tap: EventTapReader) -> Self {
        Self { core, tap }
    }

    /// The tap's rate, to compare with the graph's.
    pub(crate) fn tap_rate(&self) -> tutti_units::SampleRate {
        self.tap.rate()
    }

    /// The tap's id.
    pub(crate) fn tap_id(&self) -> tutti_graph::EventTapId {
        self.tap.id()
    }

    /// Replaces a tap the graph re-rated.
    pub(crate) fn set_tap(&mut self, tap: EventTapReader) {
        self.tap = tap;
    }

    /// What the tap held, decoded, in order; counts what it lost.
    pub(crate) fn heard(&mut self) -> Vec<Followed> {
        let core = self.core;
        let heard = self.tap.drain().filter_map(|e| core.step(e.ump)).collect();
        let lost = self.tap.take_lost();
        if lost > 0 {
            tracing::warn!("midi: the tempo follower's tap lost {lost} events");
        }
        heard
    }
}

impl FollowCx<'_> {
    /// Places what was heard at the next block start and publishes the new
    /// maps. The events say what was applied.
    pub(crate) fn apply(&self, heard: &[Followed], out: &mut Vec<crate::SessionEvent>) {
        if heard.is_empty() {
            return;
        }
        let stand = Stand::of(&self.playhead);
        let beat = stand.beat_at(&self.settings.tempo_map(), stand.next());
        self.apply_at(heard, beat, out);
    }

    /// Steps the tempo to each of `tempos` on the beat the transport reaches at
    /// its frame.
    fn apply_tempos(
        &self,
        tempos: &[(Bpm, Frame)],
        stand: &Stand,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        for &(tempo, frame) in tempos {
            let beat = stand.beat_at(&self.settings.tempo_map(), frame);
            self.apply_at(&[Followed::Tempo(tempo)], beat, out);
        }
    }

    /// As [`apply`](Self::apply), placing each change at `beat`. Each change
    /// is kept as a step over the base, then laid over the map in force, which
    /// is published once.
    fn apply_at(&self, heard: &[Followed], beat: Beat, out: &mut Vec<crate::SessionEvent>) {
        let (tempo_before, meter_before) = (self.settings.tempo_map(), self.meter.current());
        let (mut tempo, mut meter) = (Arc::clone(&tempo_before), Arc::clone(&meter_before));
        for change in heard {
            match *change {
                Followed::Tempo(tempo_set) => {
                    self.bases.tempo.push((beat, tempo_set));
                    if let Some(map) = tempo.stepped((beat, tempo_set)) {
                        tempo = Arc::new(map);
                        out.push(
                            MidiEvent::TempoFollowed {
                                tempo: tempo_set,
                                beat,
                            }
                            .into(),
                        );
                    }
                }
                Followed::Meter(signature) => {
                    self.bases.meter.push((beat, signature));
                    // The change lands on a bar line: the next one at or after `beat`.
                    let at = meter.next_bar_line(beat);
                    if let Some(map) = meter.stepped((beat, signature)) {
                        meter = Arc::new(map);
                        out.push(
                            MidiEvent::MeterFollowed {
                                signature,
                                beat: at,
                            }
                            .into(),
                        );
                    }
                }
                _ => {}
            }
        }
        if !Arc::ptr_eq(&tempo_before, &tempo) {
            self.settings.set_tempo_map(tempo);
        }
        if !Arc::ptr_eq(&meter_before, &meter) {
            self.meter.set_meter(meter);
        }
    }
}

/// A transport command and where it takes effect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placed {
    pub(crate) action: ClockAction,
    pub(crate) at: At,
}

/// What a followed clock asks of the transport, placed on the project's beats.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ClockAction {
    /// Roll from where the transport stands.
    Play,
    /// Stop where the transport stands.
    Stop,
    /// Jump to the beat, keeping whatever the transport is doing.
    Locate(Beat),
    /// Jump to the beat and roll from it.
    PlayFrom(Beat),
}

impl ClockAction {
    /// The command at the first frame of the next block, for a source that
    /// carries no arrival.
    pub(crate) fn at_next_block(self) -> Placed {
        Placed {
            action: self,
            at: At::NextBlock,
        }
    }
}

/// How long after a message arrived a command is placed: four declicks, 20 ms
/// (tutti's choice of margin), above one 60 Hz app update (16.7 ms), so a poll
/// that reads a message is usually before the frame. The lead only sets the
/// latency, a constant delay from the message; correctness is the floor in
/// [`placed_frame`], a declick past the next block, which a late poll falls
/// back to.
pub(crate) const CLOCK_LEAD: Seconds = Seconds::new(4.0 * tutti_graph::DECLICK.get());

/// The frame a command for a message that arrived at `at` takes effect on:
/// [`CLOCK_LEAD`] after the arrival, but never sooner than one
/// [`DECLICK`](tutti_graph::DECLICK) after `next`, the first frame of the next
/// block. The engine looks one declick past each block for the next command
/// (tutti-engine, "A timed command ... is seen ahead"), so a command that far on
/// is always seen with its whole fade ahead, and a poll that is late lands it at
/// `next` plus the declick and not on `next`, where it would be late.
pub(crate) fn placed_frame(at: Seconds, rate: tutti_units::SampleRate, next: Frame) -> Frame {
    Frame::nearest((at + CLOCK_LEAD).get() * rate.get())
        .max(next + tutti_graph::declick_frames(rate))
}

/// Where the transport stands at a poll, read from the playhead: what a frame
/// ahead of it is as a time and a beat.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stand {
    rate: tutti_units::SampleRate,
    frame: Frame,
    span: tutti_units::Samples,
    seconds: Seconds,
    playing: bool,
}

impl Stand {
    fn of(playhead: &Playhead) -> Self {
        Self {
            rate: playhead.rate(),
            frame: playhead.frame(),
            span: playhead.span(),
            seconds: playhead.seconds(),
            playing: playhead.playing(),
        }
    }

    /// The first frame of the next block.
    fn next(&self) -> Frame {
        self.frame + self.span
    }

    /// The transport's time on graph frame `frame`: it advances a second a
    /// second while it rolls and stands where it is while it does not.
    fn seconds_at(&self, frame: Frame) -> Seconds {
        if self.playing {
            self.seconds + (frame.to_seconds(self.rate) - self.frame.to_seconds(self.rate))
        } else {
            self.seconds
        }
    }

    /// The beat the transport is at on graph frame `frame`, through `map`
    /// (`TempoView::beat_at_frame`, never the playhead's beat extrapolated: that
    /// is one block stale and, after a ramp or a locate in the last block, wrong).
    /// The one conversion of a time to a frame.
    fn beat_at(&self, map: &TempoMap, frame: Frame) -> Beat {
        let on_map = Frame::nearest(self.seconds_at(frame).get() * self.rate.get());
        map.view(self.rate).beat_at_frame(on_map)
    }
}

/// What a clock follower said, in the unit `P` its source counts positions in:
/// a [`Beat`] for Beat Clock and [`Seconds`] for MTC.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Heard<P> {
    Tempo { tempo: Bpm, at: Seconds },
    Play,
    Stop { at: Seconds },
    Locate { position: P, at: Seconds },
    Locked,
    Unlocked,
    Slipped(ClockOffset),
    Reversed,
}

impl From<BeatClockEvent> for Heard<Beat> {
    fn from(event: BeatClockEvent) -> Self {
        match event {
            BeatClockEvent::Tempo { tempo, at } => Self::Tempo { tempo, at },
            BeatClockEvent::Play => Self::Play,
            BeatClockEvent::Stop { at } => Self::Stop { at },
            BeatClockEvent::Locate { position, at } => Self::Locate { position, at },
            BeatClockEvent::Locked => Self::Locked,
            BeatClockEvent::Unlocked => Self::Unlocked,
            BeatClockEvent::Slipped { error } => Self::Slipped(ClockOffset::Beat(error)),
        }
    }
}

impl From<MtcEvent> for Heard<Seconds> {
    fn from(event: MtcEvent) -> Self {
        match event {
            MtcEvent::Play => Self::Play,
            MtcEvent::Stop { at } => Self::Stop { at },
            MtcEvent::Locate { position, at } => Self::Locate { position, at },
            MtcEvent::Locked => Self::Locked,
            MtcEvent::Unlocked => Self::Unlocked,
            MtcEvent::Slipped { error } => Self::Slipped(ClockOffset::Time(error)),
            MtcEvent::Reversed => Self::Reversed,
        }
    }
}

/// A runtime clock follower as the session drives it: its steps, polls and
/// steers, and the transport's position in the unit it counts in.
trait Source {
    /// The unit the source counts positions in.
    type Position: Copy;
    /// The clock it reads.
    const KIND: ClockKind;

    fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(Heard<Self::Position>));
    fn poll(&mut self, now: Seconds, out: &mut impl FnMut(Heard<Self::Position>));
    fn steer(
        &mut self,
        transport: Option<Self::Position>,
        now: Seconds,
        out: &mut impl FnMut(Heard<Self::Position>),
    );
    /// Where the transport is on graph frame `frame`.
    fn transport(stand: &Stand, map: &TempoMap, frame: Frame) -> Self::Position;
    /// The beat of `position`, `lag` on: the source rolls on while a command
    /// is placed, at the tempo in force for a beat and through `map` for a time.
    fn beat_of(position: Self::Position, lag: Seconds, tempo: Bpm, map: &TempoMap) -> Beat;
}

impl Source for BeatClockFollower {
    type Position = Beat;
    const KIND: ClockKind = ClockKind::BeatClock;

    fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(Heard<Beat>)) {
        Self::step(self, ump, at, &mut |e| out(e.into()));
    }

    fn poll(&mut self, now: Seconds, out: &mut impl FnMut(Heard<Beat>)) {
        Self::poll(self, now, &mut |e| out(e.into()));
    }

    fn steer(&mut self, transport: Option<Beat>, now: Seconds, out: &mut impl FnMut(Heard<Beat>)) {
        Self::steer(self, transport, now, &mut |e| out(e.into()));
    }

    fn transport(stand: &Stand, map: &TempoMap, frame: Frame) -> Beat {
        stand.beat_at(map, frame)
    }

    fn beat_of(position: Beat, lag: Seconds, tempo: Bpm, _map: &TempoMap) -> Beat {
        position + tempo.beats_in(lag)
    }
}

impl Source for MtcFollower {
    type Position = Seconds;
    const KIND: ClockKind = ClockKind::Mtc;

    fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(Heard<Seconds>)) {
        Self::step(self, ump, at, &mut |e| out(e.into()));
    }

    fn poll(&mut self, now: Seconds, out: &mut impl FnMut(Heard<Seconds>)) {
        Self::poll(self, now, &mut |e| out(e.into()));
    }

    fn steer(
        &mut self,
        transport: Option<Seconds>,
        now: Seconds,
        out: &mut impl FnMut(Heard<Seconds>),
    ) {
        Self::steer(self, transport, now, &mut |e| out(e.into()));
    }

    fn transport(stand: &Stand, _map: &TempoMap, frame: Frame) -> Seconds {
        stand.seconds_at(frame)
    }

    fn beat_of(position: Seconds, lag: Seconds, _tempo: Bpm, map: &TempoMap) -> Beat {
        map.beat_at_seconds(Seconds::new(position.get() + lag.get()))
    }
}

/// What a poll's events ask of the transport, placed, and the tempos to step.
/// Reports go to `out`. `rolling` is whether the source is rolling after them;
/// a locate while it is not is not advanced.
fn lay<S: Source>(
    events: Vec<Heard<S::Position>>,
    rolling: bool,
    stand: &Stand,
    tempo: Bpm,
    map: &TempoMap,
    out: &mut Vec<crate::SessionEvent>,
) -> (Vec<Placed>, Vec<(Bpm, Frame)>) {
    let kind = S::KIND;
    let rate = stand.rate;
    let next = stand.next();
    let placed = |at: Seconds| placed_frame(at, rate, next);
    // Where the source is on the frame `at` is placed on.
    let beat_of = |position: S::Position, at: Seconds| {
        let lag = if rolling {
            Seconds::new((placed(at).to_seconds(rate) - at).get().max(0.0))
        } else {
            Seconds::new(0.0)
        };
        S::beat_of(position, lag, tempo, map)
    };
    let mut tempos = Vec::new();
    let mut actions: Vec<Placed> = Vec::new();
    // A bare Play lands where the command before it did.
    let mut last = At::Frame(next + tutti_graph::declick_frames(rate));
    let mut events = events.into_iter().peekable();
    while let Some(event) = events.next() {
        match event {
            Heard::Locate { position, at } => {
                let beat = beat_of(position, at);
                last = At::Frame(placed(at));
                let action = if events.next_if(|e| matches!(e, Heard::Play)).is_some() {
                    ClockAction::PlayFrom(beat)
                } else {
                    ClockAction::Locate(beat)
                };
                actions.push(Placed { action, at: last });
            }
            Heard::Play => actions.push(Placed {
                action: ClockAction::Play,
                at: last,
            }),
            Heard::Stop { at } => {
                last = At::Frame(placed(at));
                actions.push(Placed {
                    action: ClockAction::Stop,
                    at: last,
                });
            }
            Heard::Tempo { tempo, at } => tempos.push((tempo, placed(at))),
            Heard::Locked => out.push(MidiEvent::ClockLocked { kind }.into()),
            Heard::Unlocked => out.push(MidiEvent::ClockUnlocked { kind }.into()),
            Heard::Slipped(error) => {
                out.push(MidiEvent::ClockSlipped { kind, error }.into());
            }
            Heard::Reversed => out.push(MidiEvent::ClockReversed { kind }.into()),
        }
    }
    (actions, tempos)
}

/// The follower of the kind the app picked, holding only that kind's state.
enum Clock {
    BeatClock(BeatClockFollower),
    Mtc(MtcFollower),
}

/// The one clock follower a session has: its core and its tap. A Beat Clock
/// follower lays its tempo over the base; MTC carries no tempo.
pub(crate) struct ClockFollow {
    core: Clock,
    tap: EventTapReader,
    /// Whether the source has said Play and not Stop since.
    rolling: bool,
}

impl ClockFollow {
    /// A follower of `kind` reading `tap`.
    pub(crate) fn new(kind: ClockKind, tap: EventTapReader) -> Self {
        Self {
            core: match kind {
                ClockKind::BeatClock => Clock::BeatClock(BeatClockFollower::new()),
                ClockKind::Mtc => Clock::Mtc(MtcFollower::new()),
            },
            tap,
            rolling: false,
        }
    }

    /// Whether it steps the tempo, so nothing else follows one.
    pub(crate) fn owns_tempo(&self) -> bool {
        matches!(self.core, Clock::BeatClock(_))
    }

    /// The error the follower read at the last poll
    /// ([`BeatClockFollower::error`], [`MtcFollower::error`]).
    pub(crate) fn error(&self) -> Option<ClockOffset> {
        match &self.core {
            Clock::BeatClock(f) => f.error().map(ClockOffset::Beat),
            Clock::Mtc(f) => f.error().map(ClockOffset::Time),
        }
    }

    /// The tap's rate, to compare with the graph's.
    pub(crate) fn tap_rate(&self) -> tutti_units::SampleRate {
        self.tap.rate()
    }

    /// The tap's id.
    pub(crate) fn tap_id(&self) -> tutti_graph::EventTapId {
        self.tap.id()
    }

    /// Replaces a tap the graph re-rated.
    pub(crate) fn set_tap(&mut self, tap: EventTapReader) {
        self.tap = tap;
    }

    /// Reads the tap, publishes the tempo it heard and reports lock changes
    /// into `out`; what the transport is asked to do, placed on beats, in order.
    pub(crate) fn heard(
        &mut self,
        cx: &FollowCx<'_>,
        out: &mut Vec<crate::SessionEvent>,
    ) -> Vec<Placed> {
        let rate = self.tap.rate();
        let instant = |frame: Frame| frame.to_seconds(rate);
        let arrived: Vec<(Ump, Seconds)> = self
            .tap
            .drain()
            .map(|event| (event.ump, instant(event.frame)))
            .collect();
        let lost = self.tap.take_lost();
        if lost > 0 {
            tracing::warn!("midi: the clock follower's tap lost {lost} events");
        }
        let now = instant(cx.playhead.frame());
        let stand = Stand::of(&cx.playhead);
        let map = cx.settings.tempo_map();
        let rolling = self.rolling;
        let (actions, tempos, rolling) = match &mut self.core {
            Clock::BeatClock(f) => drive(f, &arrived, now, rolling, cx, &stand, &map, out),
            Clock::Mtc(f) => drive(f, &arrived, now, rolling, cx, &stand, &map, out),
        };
        self.rolling = rolling;
        if self.owns_tempo() {
            cx.apply_tempos(&tempos, &stand, out);
        }
        actions
    }
}

/// One poll of `source`: the messages that `arrived`, the silence to `now`, and
/// a steer against the transport. The placed commands, the tempos to step and
/// whether the source is rolling after them.
#[allow(clippy::too_many_arguments)]
fn drive<S: Source>(
    source: &mut S,
    arrived: &[(Ump, Seconds)],
    now: Seconds,
    was_rolling: bool,
    cx: &FollowCx<'_>,
    stand: &Stand,
    map: &TempoMap,
    out: &mut Vec<crate::SessionEvent>,
) -> (Vec<Placed>, Vec<(Bpm, Frame)>, bool) {
    let mut events = Vec::new();
    for &(ump, at) in arrived {
        source.step(ump, at, &mut |e| events.push(e));
    }
    source.poll(now, &mut |e| events.push(e));
    let rolling = events
        .iter()
        .rev()
        .find_map(|e| match e {
            Heard::Play => Some(true),
            Heard::Stop { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(was_rolling);
    // The drift between the source and the transport, read at the frame a
    // command lands on; a transport that is not rolling has none.
    let steered =
        (rolling && cx.playhead.playing()).then(|| S::transport(stand, map, stand.next()));
    let lands = stand.next().to_seconds(stand.rate);
    source.steer(steered, lands, &mut |e| events.push(e));
    let (actions, tempos) = lay::<S>(
        events,
        rolling,
        stand,
        cx.settings.current_tempo(),
        map,
        out,
    );
    (actions, tempos, rolling)
}

/// The one Machine Control follower a session has: its core and its tap.
pub(crate) struct MachineFollow {
    core: MachineControlFollower,
    tap: EventTapReader,
}

impl MachineFollow {
    /// A follower of `core` reading `tap`.
    pub(crate) fn new(core: MachineControlFollower, tap: EventTapReader) -> Self {
        Self { core, tap }
    }

    /// The tap's rate, to compare with the graph's.
    pub(crate) fn tap_rate(&self) -> tutti_units::SampleRate {
        self.tap.rate()
    }

    /// The tap's id.
    pub(crate) fn tap_id(&self) -> tutti_graph::EventTapId {
        self.tap.id()
    }

    /// Replaces a tap the graph re-rated.
    pub(crate) fn set_tap(&mut self, tap: EventTapReader) {
        self.tap = tap;
    }

    /// Reads the tap and reports the commands it did not act on into `out`;
    /// what the transport is asked to do, placed on beats through the tempo
    /// map, in order. A locate followed by a play in one message is one
    /// [`ClockAction::PlayFrom`].
    pub(crate) fn heard(
        &mut self,
        cx: &FollowCx<'_>,
        out: &mut Vec<crate::SessionEvent>,
    ) -> Vec<ClockAction> {
        let mut events = Vec::new();
        for event in self.tap.drain() {
            self.core.step(event.ump, &mut |e| events.push(e));
        }
        let lost = self.tap.take_lost();
        if lost > 0 {
            tracing::warn!("midi: the Machine Control follower's tap lost {lost} events");
        }
        let map = cx.settings.tempo_map();
        let mut actions = Vec::new();
        let mut events = events.into_iter().peekable();
        while let Some(event) = events.next() {
            match event {
                MachineControlEvent::Locate(time) => {
                    let beat = map.beat_at_seconds(time);
                    if events
                        .next_if(|e| *e == MachineControlEvent::Play)
                        .is_some()
                    {
                        actions.push(ClockAction::PlayFrom(beat));
                    } else {
                        actions.push(ClockAction::Locate(beat));
                    }
                }
                MachineControlEvent::Play => actions.push(ClockAction::Play),
                MachineControlEvent::Stop => actions.push(ClockAction::Stop),
                MachineControlEvent::Ignored(command) => {
                    out.push(MidiEvent::MachineControlIgnored(command).into());
                }
                MachineControlEvent::Segmentation(error) => {
                    out.push(MidiEvent::MachineControlSegmentation(error).into());
                }
                // An event this session has no counterpart for.
                _ => {}
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests;
