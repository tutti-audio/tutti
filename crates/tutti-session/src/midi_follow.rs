//! Following an outside tempo (feature `midi`): the session's transport takes the tempo and the
//! meter a MIDI 2.0 source sends as Flex Data Set Tempo and Set Time Signature
//! (M2-104 §7.5.3, §7.5.4).
//!
//! The pure core is `tutti_midi_runtime::TempoFollower`: it decodes and says
//! *what* to apply. This module says *where* and *how*: it drains an event tap
//! on the followed Group's channelless port of the input node in
//! [`Phase::Poll`](crate::Phase::Poll), places each change at the next block
//! start, builds the new [`TempoMap`] and [`MeterMap`] and publishes them.
//!
//! **A follow mode overlays the project's maps and never replaces them.** The
//! maps the project had when following began are kept apart; the published map
//! is the project's up to the beat of the first change, then the live steps.
//! Each live step truncates what came after it: a project point or ramp ahead
//! of the playhead is not played while following, because two authorities for
//! the same future beats is the bug to avoid. A ramp a step lands in is cut at
//! that beat. [`Midi::unfollow_tempo`](crate::Midi::unfollow_tempo) publishes the
//! project's maps again; the clock re-seats at its current beat, so the
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
//! change always begins a bar; M2-104 §7.5.4), on the same overlay.
//!
//! # Following a MIDI clock
//!
//! [`Midi::follow_clock`](crate::Midi::follow_clock) follows MIDI Beat Clock or
//! MIDI Time Code from one Group the same way: a tap on its channelless port,
//! drained in `Phase::Poll`, read by the pure
//! `tutti_midi_runtime::ClockFollower` with each event's arrival on the graph's
//! frame clock: seconds since graph frame 0 (the tap's frame over its rate). A tempo it fits goes through the
//! overlay above, as a Flex Set Tempo does. Play, stop and locate go to the
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
//! (`ClockFollower::steer`): a beat for Beat Clock, a time for MTC. For Beat
//! Clock the error against the source's beat is closed by a trim of the tempo
//! published through the overlay; for either kind an error past
//! `ClockFollower::SLIP` (Beat Clock) or `ClockFollower::MTC_SLIP_FRAMES` (MTC)
//! locates to the source and is reported ([`MidiReport::ClockSlipped`]). The
//! latest error is [`Midi::clock_error`](crate::Midi::clock_error).
//!
//! **MTC running backward** is reported ([`MidiReport::ClockReversed`]) and the
//! transport is stopped: it has no negative rate, so a reverse source cannot be
//! followed, and the session takes no position from it until its Quarter Frames
//! ascend again. **A source off its nominal speed** is followed by locates alone
//! (no rate control exists on the transport); the bound is in
//! `tutti_midi_runtime::ClockFollower`'s MTC rules.
//!
//! MTC `00:00:00:00` is beat 0 of the project's tempo map: a position in time
//! is a beat through the map in force.

use std::sync::Arc;

use tutti_engine::Playhead;
use tutti_graph::EventTapReader;
use tutti_midi_protocol::SysexDeviceId;
use tutti_midi_runtime::{
    ClockEvent, ClockFollower, ClockKind, ClockOffset, ClockPosition, Followed,
    MachineControlEvent, MachineControlFollower, TempoFollower,
};
use tutti_music::MeterMap;
use tutti_tempo::TempoMap;
use tutti_timeline::{ProjectMeter, TransportSettings};
use tutti_ump::MidiGroup;
use tutti_units::{At, Beat, Bpm, Frame, Seconds};

use crate::MidiReport;

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
    /// The input port followed: its input node
    /// (`MidiInput::node`).
    pub input: tutti_graph::NodeKey,
    /// The Group followed. A Group is a cable of its own (M2-104 §2.1.2,
    /// Appendix B.1); the Group must be one the input hears
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    pub group: MidiGroup,
    /// Whether Set Time Signature is followed too.
    pub meter: bool,
}

/// How many events a clock follower's tap holds between two polls: Timing
/// Clocks come at up to 120 a second (300 bpm) and Quarter Frames at 120, so a
/// poll interval of several seconds is held.
pub(crate) const CLOCK_TAP_CAPACITY: tutti_graph::EventCapacity =
    tutti_graph::EventCapacity::of(1024);

/// What [`Midi::follow_clock`](crate::Midi::follow_clock) follows.
///
/// One Group of an input port
/// (`MidiInput::node`) is read for one kind of
/// clock. A device that sends both Beat Clock and MTC is followed for one: the
/// session has one transport and one authority for its position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowClock {
    /// The input port followed: its input node
    /// (`MidiInput::node`).
    pub input: tutti_graph::NodeKey,
    /// The Group followed. A Group is a cable of its own (M2-104 §2.1.2,
    /// Appendix B.1); the Group must be one the input hears
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    pub group: MidiGroup,
    /// Which clock it sends.
    pub kind: ClockKind,
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
    /// The input port followed: its input node
    /// (`MidiInput::node`).
    pub input: tutti_graph::NodeKey,
    /// The Group followed. A Group is a cable of its own (M2-104 §2.1.2,
    /// Appendix B.1); the Group must be one the input hears
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    pub group: MidiGroup,
    /// This session's MMC device ID. A command to it, to a group it was assigned
    /// to, or to the all-call (`7F`, always accepted) is followed.
    pub device: SysexDeviceId,
}

/// The maps a follower overlays: the project's, and what the follower has
/// published over them.
struct Overlay {
    /// The tempo map the project had when following began: what an unfollow
    /// publishes again. The session has no project tempo apart from the
    /// published map, so this is the map it found.
    project_tempo: Arc<TempoMap>,
    /// The project's meter, likewise.
    project_meter: Arc<MeterMap>,
    /// The map this follower last published (the project's until a first
    /// step): what the next step builds on.
    tempo: Arc<TempoMap>,
    meter: Arc<MeterMap>,
}

/// The one tempo follower a session has: its core, its tap and the maps.
pub(crate) struct Follow {
    core: TempoFollower,
    tap: EventTapReader,
    overlay: Overlay,
}

/// What a `Poll` phase reads besides the follower.
pub(crate) struct FollowCx<'a> {
    pub(crate) settings: &'a TransportSettings,
    pub(crate) meter: &'a ProjectMeter,
    pub(crate) playhead: Playhead,
}

impl Overlay {
    /// An overlay over the project's maps, with nothing published yet.
    fn new(project_tempo: Arc<TempoMap>, project_meter: Arc<MeterMap>) -> Self {
        Self {
            tempo: Arc::clone(&project_tempo),
            meter: Arc::clone(&project_meter),
            project_tempo,
            project_meter,
        }
    }

    /// What the project had, to publish again, for the maps this overlay
    /// changed (`None` for one it never touched).
    fn restore(&self) -> (Option<Arc<TempoMap>>, Option<Arc<MeterMap>>) {
        (
            (!Arc::ptr_eq(&self.tempo, &self.project_tempo))
                .then(|| Arc::clone(&self.project_tempo)),
            (!Arc::ptr_eq(&self.meter, &self.project_meter))
                .then(|| Arc::clone(&self.project_meter)),
        )
    }
}

impl Follow {
    /// A follower reading `tap`, over the project's maps.
    pub(crate) fn new(
        core: TempoFollower,
        tap: EventTapReader,
        project_tempo: Arc<TempoMap>,
        project_meter: Arc<MeterMap>,
    ) -> Self {
        Self {
            core,
            tap,
            overlay: Overlay::new(project_tempo, project_meter),
        }
    }

    /// A follower reading `tap` in place of this one: the project's maps and the
    /// overlay published so far carry over, so a second source replaces the
    /// first with the tempo in force staying until it sends one.
    pub(crate) fn replaced_by(self, core: TempoFollower, tap: EventTapReader) -> Self {
        Self { core, tap, ..self }
    }

    /// What the project had, to publish again, for the maps this follower
    /// changed (`None` for one it never touched).
    pub(crate) fn restore(&self) -> (Option<Arc<TempoMap>>, Option<Arc<MeterMap>>) {
        self.overlay.restore()
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

    /// Places what was heard at the next block start and publishes the new
    /// maps. The events say what was applied.
    pub(crate) fn apply(
        &mut self,
        heard: &[Followed],
        cx: &FollowCx<'_>,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        self.overlay.apply(heard, cx, out);
    }
}

impl Overlay {
    /// Places what was heard at the next block start and publishes the new
    /// maps. The events say what was applied.
    fn apply(&mut self, heard: &[Followed], cx: &FollowCx<'_>, out: &mut Vec<crate::SessionEvent>) {
        if heard.is_empty() {
            return;
        }
        let stand = Stand::of(&cx.playhead);
        let beat = stand.beat_at(&self.tempo, stand.next());
        self.apply_at(heard, beat, cx, out);
    }

    /// Steps the tempo to each of `tempos` on the beat the transport reaches at
    /// its frame.
    fn apply_tempos(
        &mut self,
        tempos: &[(Bpm, Frame)],
        stand: &Stand,
        cx: &FollowCx<'_>,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        for &(tempo, frame) in tempos {
            let beat = stand.beat_at(&self.tempo, frame);
            self.apply_at(&[Followed::Tempo(tempo)], beat, cx, out);
        }
    }

    /// As [`apply`](Self::apply), placing each change at `beat`.
    fn apply_at(
        &mut self,
        heard: &[Followed],
        beat: Beat,
        cx: &FollowCx<'_>,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        let (tempo_before, meter_before) = (Arc::clone(&self.tempo), Arc::clone(&self.meter));
        for change in heard {
            match *change {
                Followed::Tempo(tempo) => match self.tempo.stepped_to(beat, tempo) {
                    Ok(Some(map)) => {
                        self.tempo = Arc::new(map);
                        out.push(MidiReport::TempoFollowed { tempo, beat }.into());
                    }
                    Ok(None) => {}
                    // The builder refuses only a tempo whose segment duration overflows
                    // an `f64` (below ~1e-300 bpm); a `Bpm` from `flex_tempo` is at least
                    // 1.397 bpm (field `u32::MAX`), so this is not reached: nothing is
                    // published and nothing reported.
                    Err(_) => {}
                },
                Followed::Meter(signature) => {
                    // `at` is a bar line: it comes from `next_bar_line`.
                    let at = self.meter.next_bar_line(beat);
                    if let Some(map) = self.meter.with_change_at(at, signature) {
                        self.meter = Arc::new(map);
                        out.push(
                            MidiReport::MeterFollowed {
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
        if !Arc::ptr_eq(&tempo_before, &self.tempo) {
            cx.settings.set_tempo_map(Arc::clone(&self.tempo));
        }
        if !Arc::ptr_eq(&meter_before, &self.meter) {
            cx.meter.set_meter(Arc::clone(&self.meter));
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

/// What a poll's events ask of the transport, placed, and the tempos to step.
/// Reports go to `out`. `rolling` is whether the source is rolling after them;
/// a locate while it is not is not advanced.
fn lay(
    events: Vec<ClockEvent>,
    kind: ClockKind,
    rolling: bool,
    stand: &Stand,
    tempo: Bpm,
    map: &TempoMap,
    out: &mut Vec<crate::SessionEvent>,
) -> (Vec<Placed>, Vec<(Bpm, Frame)>) {
    let rate = stand.rate;
    let next = stand.next();
    let placed = |at: Seconds| placed_frame(at, rate, next);
    // Where the source is on the frame `at` is placed on.
    let beat_of = |position: ClockPosition, at: Seconds| {
        let lag = if rolling {
            (placed(at).to_seconds(rate) - at).get().max(0.0)
        } else {
            0.0
        };
        match position {
            ClockPosition::Beat(beat) => beat + tempo.beats_in(Seconds::new(lag)),
            ClockPosition::Time(time) => map.beat_at_seconds(Seconds::new(time.get() + lag)),
        }
    };
    let mut tempos = Vec::new();
    let mut actions: Vec<Placed> = Vec::new();
    // A bare Play lands where the command before it did.
    let mut last = At::Frame(next + tutti_graph::declick_frames(rate));
    let mut events = events.into_iter().peekable();
    while let Some(event) = events.next() {
        match event {
            ClockEvent::Locate { position, at } => {
                let beat = beat_of(position, at);
                last = At::Frame(placed(at));
                let action = if events.next_if(|e| *e == ClockEvent::Play).is_some() {
                    ClockAction::PlayFrom(beat)
                } else {
                    ClockAction::Locate(beat)
                };
                actions.push(Placed { action, at: last });
            }
            ClockEvent::Play => actions.push(Placed {
                action: ClockAction::Play,
                at: last,
            }),
            ClockEvent::Stop { at } => {
                last = At::Frame(placed(at));
                actions.push(Placed {
                    action: ClockAction::Stop,
                    at: last,
                });
            }
            ClockEvent::Tempo { tempo, at } => tempos.push((tempo, placed(at))),
            ClockEvent::Locked => out.push(MidiReport::ClockLocked { kind }.into()),
            ClockEvent::Unlocked => out.push(MidiReport::ClockUnlocked { kind }.into()),
            ClockEvent::Slipped { error } => {
                out.push(MidiReport::ClockSlipped { kind, error }.into());
            }
            ClockEvent::Reversed => out.push(MidiReport::ClockReversed { kind }.into()),
        }
    }
    (actions, tempos)
}

/// The one clock follower a session has: its core, its tap and, for Beat Clock,
/// the tempo overlay.
pub(crate) struct ClockFollow {
    core: ClockFollower,
    tap: EventTapReader,
    /// The maps the clock's tempo is published over; `None` for MTC, which
    /// carries no tempo.
    overlay: Option<Overlay>,
    /// Whether the source has said Play and not Stop since.
    rolling: bool,
}

impl ClockFollow {
    /// A follower of `core` reading `tap`. A Beat Clock follower overlays the
    /// project's maps; an MTC one leaves them.
    pub(crate) fn new(
        core: ClockFollower,
        tap: EventTapReader,
        project_tempo: Arc<TempoMap>,
        project_meter: Arc<MeterMap>,
    ) -> Self {
        let overlay = (core.kind() == ClockKind::BeatClock)
            .then(|| Overlay::new(project_tempo, project_meter));
        Self {
            core,
            tap,
            overlay,
            rolling: false,
        }
    }

    /// Whether it publishes a tempo, so nothing else may.
    pub(crate) fn owns_tempo(&self) -> bool {
        self.overlay.is_some()
    }

    /// What the project had, to publish again, for the maps this follower
    /// changed.
    pub(crate) fn restore(&self) -> (Option<Arc<TempoMap>>, Option<Arc<MeterMap>>) {
        self.overlay.as_ref().map_or((None, None), Overlay::restore)
    }

    /// The error the follower read at the last poll
    /// ([`ClockFollower::error`]).
    pub(crate) fn error(&self) -> Option<ClockOffset> {
        self.core.error()
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
        let mut events = Vec::new();
        for event in self.tap.drain() {
            self.core
                .step(event.ump, instant(event.frame), &mut |e| events.push(e));
        }
        let lost = self.tap.take_lost();
        if lost > 0 {
            tracing::warn!("midi: the clock follower's tap lost {lost} events");
        }
        self.core
            .poll(instant(cx.playhead.frame()), &mut |e| events.push(e));

        let stand = Stand::of(&cx.playhead);
        let lands = instant(stand.next());
        let rolling = events
            .iter()
            .rev()
            .find_map(|e| match e {
                ClockEvent::Play => Some(true),
                ClockEvent::Stop { .. } => Some(false),
                _ => None,
            })
            .unwrap_or(self.rolling);
        self.rolling = rolling;
        let map = cx.settings.tempo_map();
        // The drift between the source and the transport, read at the frame a
        // command lands on; a transport that is not rolling has none.
        let steered = (rolling && cx.playhead.playing()).then(|| match self.core.kind() {
            ClockKind::BeatClock => ClockPosition::Beat(stand.beat_at(&map, stand.next())),
            ClockKind::Mtc => ClockPosition::Time(stand.seconds_at(stand.next())),
        });
        self.core.steer(steered, lands, &mut |e| events.push(e));
        let (actions, tempos) = lay(
            events,
            self.core.kind(),
            rolling,
            &stand,
            cx.settings.current_tempo(),
            &map,
            out,
        );
        if let Some(overlay) = &mut self.overlay {
            overlay.apply_tempos(&tempos, &stand, cx, out);
        }
        actions
    }
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
                    out.push(MidiReport::MachineControlIgnored(command).into());
                }
                MachineControlEvent::Segmentation(error) => {
                    out.push(MidiReport::MachineControlSegmentation(error).into());
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
