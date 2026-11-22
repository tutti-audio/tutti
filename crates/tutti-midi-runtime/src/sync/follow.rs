//! [`ClockFollower`]: what a transport that follows an outside MIDI clock does,
//! as a pure state machine.
//!
//! One follower reads one source's UMPs, each with the time it arrived, and
//! says what the source asks of a transport: a tempo, play and stop, a locate,
//! and whether it is locked to the source ([`ClockEvent`]). It holds no clock
//! and no transport: whoever owns the transport places each event (the session,
//! in `tutti-session`).
//!
//! **Time** is [`Seconds`] on one clock, whose zero the caller picks (the
//! session's is graph frame 0): every `at` and `now` of a follower is read on
//! it, and only differences are read. It is the base of [`MidiClockDecoder`] and
//! [`MtcDecoder`] too, so a source stamped by a hardware clock and one stamped
//! by the graph's frame count take the same call.
//!
//! A follower is one of two state machines, one per [`ClockKind`], each holding
//! only its own state.
//!
//! # Beat Clock
//!
//! [`ClockKind::BeatClock`] (`BeatClockFollower`) reads System Real-Time Timing Clock, Start, Continue
//! and Stop and the System Common Song Position Pointer (M1 pp.27 to 31), through
//! [`MidiClockDecoder`].
//!
//! - **Tempo** is the decoder's least-squares fit of the tick spacing. It is
//!   announced when the clock locks, and again only when it has moved by
//!   [`TEMPO_STEP`](ClockFollower::TEMPO_STEP) and a quarter note of ticks has
//!   passed since the last: a source whose estimate wanders by jitter grows a
//!   tempo map by one point a beat at most, and by none while it holds still.
//! - **Start** locates to beat 0 and plays; **Continue** locates to where the
//!   decoder counted to and plays; **Stop** stops and keeps the position.
//! - **Song Position Pointer** locates to its value in sixteenth notes: six
//!   clocks is a sixteenth, so a pointer of `n` is beat `n / 4`.
//! - The clock is **locked** per [`MidiClockDecoder::locked`], and **unlocked**
//!   by a gap in the ticks or by [`MidiClockDecoder::DROPOUT`] of silence, which
//!   also stops a transport the source had started.
//!
//! # Drift
//!
//! A tempo announced only when it moves by [`TEMPO_STEP`](ClockFollower::TEMPO_STEP)
//! leaves a tempo the source wanders by less than that unfollowed, and a transport
//! that was located a block late stays a block late: the beat error `e`, the
//! source's beat less the transport's, grows between locates. The owner of the
//! transport reads its beat and gives it to [`steer`](ClockFollower::steer),
//! which closes `e` by a trim of the fitted tempo:
//!
//! - `e` is the source's beat at `now`, the ticks counted plus the time since the
//!   last at the fitted tempo, less the transport's beat at `now`.
//! - The trim is the tempo that closes `e` in [`CATCH_UP`](ClockFollower::CATCH_UP):
//!   `e * 60 / CATCH_UP` bpm, at most 2% of the
//!   fitted tempo either way. The trimmed tempo is announced by the rule above,
//!   so a trim is a tempo step of at least `TEMPO_STEP` and a map grows by one
//!   point a beat at most.
//! - A tempo step is the smallest correction, so an error below
//!   `TEMPO_STEP * CATCH_UP / 60` beats is left: with the tutti constants,
//!   `0.1 * 4 / 60`, about 0.0067 beat. Over one beat of
//!   ticks, `LOCK_TICKS` (the least between announcements), the error grows by
//!   at most `TEMPO_STEP / 60` beats a second, a beat's duration at the source's
//!   tempo, so the error stays within the bound above plus that and the fit's
//!   own noise.
//! - An error past [`SLIP`](ClockFollower::SLIP) is not drift: the follower says
//!   [`ClockEvent::Slipped`] and locates to the source's beat, once, and holds off
//!   for [`SETTLE`](ClockFollower::SETTLE) while the command lands (as it does after any locate).
//!
//! MTC has no tempo to trim and the transport has no rate to follow a source's
//! speed by: [`steer`](ClockFollower::steer) only locates it (below).
//!
//! # MIDI Time Code
//!
//! [`ClockKind::Mtc`] (`MtcFollower`) reads Quarter Frames through [`MtcDecoder`] and the Full
//! Message (Universal Real Time `01 01`, M1 p.53 and T-9) through
//! `tutti-midi-protocol`'s reader. MTC has no Start and no Stop: time is running
//! once Quarter Frames arrive after a Full Message, and stopped when they stop.
//!
//! - A completed cycle of eight Quarter Frames names a frame: the one that
//!   started at the arrival of its piece 0 (M1 MTC, Quarter Frame Message
//!   Implementation: piece 0 "must be sent on a frame boundary"). By the last
//!   piece the source is 1.75 frames on, which the spec rounds to the receiver's
//!   +2 frame offset; the follower keeps the frame and the arrival of piece 0
//!   instead, so a [`Locate`](ClockEvent::Locate) says where the source was at
//!   that arrival and whoever applies it advances by the time since, as for any
//!   locate. A cycle is **consistent** when it names a frame a whole number of
//!   cycles (two frames each) after the one before at the same rate, within
//!   [`MTC_DROPOUT`](ClockFollower::MTC_DROPOUT): one cycle is what a generator
//!   running forward sends, more is a cycle the assembler discarded, and neither
//!   needs an arrival time to judge. Any other step is a jump.
//! - The follower is **locked** after [`MTC_LOCK_CYCLES`](ClockFollower::MTC_LOCK_CYCLES)
//!   consistent cycles, and plays, locating to the position first. A cycle that
//!   is not consistent while it plays is a jump of the source, and locates.
//! - A Full Message **locates** and leaves the motion as it was: the generator
//!   pauses for the receiver to reach the position and resumes with Quarter
//!   Frames. The next cycle after it is taken as a first one, not a jump.
//! - **Unlocked** when [`MTC_DROPOUT`](ClockFollower::MTC_DROPOUT) passes with no
//!   Quarter Frame or Full Message, which also stops the transport.
//! - **Slip.** [`steer`](ClockFollower::steer) is given the transport's time. The
//!   error is the source's time (the last cycle's frame, plus the time since its
//!   piece 0) less the transport's. Past
//!   [`MTC_SLIP_FRAMES`](ClockFollower::MTC_SLIP_FRAMES) it says
//!   [`ClockEvent::Slipped`] and locates to the source's time, once, and holds
//!   off for [`SETTLE`](ClockFollower::SETTLE) as Beat Clock does.
//! - **Varispeed.** A source off its nominal speed shows in the spacing of its
//!   Quarter Frames. The transport has no rate to follow it with, so it is
//!   followed by locates alone: for a source `d` (a fraction) fast or slow of
//!   nominal, the error grows by `|d|` a second from each locate, so it is
//!   located back each `max(SETTLE, T / |d|)` seconds, `T` the threshold
//!   `MTC_SLIP_FRAMES / fps` (80 ms at 25 fps, 83 ms at 24 and 67 ms at 30).
//!   The error is read at most `max(T, |d| * SETTLE)` plus `|d|` times a cycle
//!   and a control tick (the last cycle's extrapolation falls short by `|d|`
//!   times its age): at 25 fps and a 10 ms tick, a source 2% off is 80 ms off at
//!   the worst and located every 4 s, and one 10% off is 100 ms off and located
//!   every second.
//! - **Reverse.** Quarter Frames in descending piece order (`7` to `0`) mean the
//!   source runs backward. It is read from [`MtcDecoder::DIRECTION_STEPS`] steps in a row
//!   ([`MtcDecoder::direction`]), so one glitch is not a reverse and said once as [`ClockEvent::Reversed`]. The
//!   transport has no negative rate, so the follower stops it, unlocks, and takes
//!   no position until the pieces ascend again, when it locks afresh.

use tutti_midi_protocol::{
    Addressed, SmpteFrameRate, SmpteTimecode, Universal, UniversalRead, read,
};
use tutti_ump::{Message, SysexAssembler, SysexStep, System, Ump};
use tutti_units::{Beat, Bpm, Seconds, TimecodeFrames};

use super::{ClockTransportState, MidiClockDecoder, MtcDecoder, MtcDirection};

/// Frames in one Quarter Frame cycle: the step between two consistent cycles.
const CYCLE_FRAMES: u64 = 2;

/// Which outside clock a [`ClockFollower`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClockKind {
    /// MIDI Beat Clock: Timing Clock, Start, Continue, Stop and Song Position
    /// Pointer. A musical position and a tempo.
    BeatClock,
    /// MIDI Time Code: Quarter Frames and the Full Message. A position in
    /// time, and no tempo.
    Mtc,
}

/// A position a source locates to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockPosition {
    /// A musical position: Beat Clock counts beats.
    Beat(Beat),
    /// A position in time since `00:00:00:00`: MTC counts frames.
    Time(Seconds),
}

/// What a source asks of the transport, or says of its own state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockEvent {
    /// The tempo the source's clock fits, in beats per minute, as of the
    /// Timing Clock that announced it.
    Tempo {
        /// The tempo.
        tempo: Bpm,
        /// When that Timing Clock arrived, on the follower's clock.
        at: Seconds,
    },
    /// Roll from the position (a [`Locate`](Self::Locate) comes first when the
    /// source says where).
    Play,
    /// Stop where the transport stands, as of `at`: the arrival of the message
    /// that said so, or the poll that found the source silent.
    Stop {
        /// When the source stopped, on the follower's clock.
        at: Seconds,
    },
    /// The source is at `position` as of `at`, the arrival of the message that
    /// said so. A transport that applies it later is `later - at` behind a
    /// source that kept rolling.
    Locate {
        /// Where the source is.
        position: ClockPosition,
        /// When the message arrived, on the follower's clock.
        at: Seconds,
    },
    /// The follower locked to the source.
    Locked,
    /// The follower lost the source: a gap, or silence for the kind's dropout.
    Unlocked,
    /// The transport was further than [`ClockFollower::SLIP`] (Beat Clock) or
    /// [`ClockFollower::MTC_SLIP_FRAMES`] (MTC) from the source
    /// ([`ClockFollower::steer`]): a [`Locate`](Self::Locate) to the source's
    /// position follows at once.
    Slipped {
        /// The source's position less the transport's, when it was found.
        error: ClockOffset,
    },
    /// The source's time code runs backward: its Quarter Frames arrive in
    /// descending piece order (MTC, Quarter Frame Message Implementation). The
    /// transport has no negative rate, so the follower stops it and unlocks (a
    /// [`Stop`](Self::Stop) and an [`Unlocked`](Self::Unlocked) follow when the
    /// source was locked) and takes no position until the source runs forward
    /// again.
    Reversed,
}

/// How far the transport is from the source, in the unit the source counts in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockOffset {
    /// Beat Clock: the source's beat less the transport's.
    Beat(Beat),
    /// MTC: the source's time less the transport's.
    Time(Seconds),
}

/// Follows one outside MIDI clock; the rules are in the module docs.
///
/// ```
/// use tutti_midi_runtime::{ClockEvent, ClockFollower, ClockKind};
/// use tutti_units::{Bpm, Seconds};
/// use tutti_ump::{Message, MidiGroup, System, Ump};
///
/// let mut follower = ClockFollower::new(ClockKind::BeatClock);
/// let tick = Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock));
/// let mut heard = Vec::new();
/// // 120 bpm is 24 ticks per half second.
/// for i in 0..48 {
///     let at = Seconds::new(f64::from(i) * 0.5 / 24.0);
///     follower.step(tick, at, &mut |e| heard.push(e));
/// }
/// assert!(heard.contains(&ClockEvent::Locked));
/// assert!(heard.iter().any(|e| matches!(e, ClockEvent::Tempo { tempo, .. } if (tempo.get() - 120.0).abs() < 1e-6)));
/// ```
#[derive(Debug)]
pub struct ClockFollower(Follower);

/// The follower of a kind: the state of that kind alone.
#[derive(Debug)]
enum Follower {
    BeatClock(BeatClockFollower),
    Mtc(MtcFollower),
}

/// The Beat Clock state machine.
#[derive(Debug)]
struct BeatClockFollower {
    decoder: MidiClockDecoder,
    /// The tempo last announced, and the ticks since.
    announced: Option<Bpm>,
    ticks_since_announce: usize,
    locked: bool,
    /// When the last tick arrived.
    last_tick: Option<Seconds>,
    /// The trim [`steer`](ClockFollower::steer) last asked, as a fraction of the
    /// fitted tempo: the announced tempo is the fit times one plus this.
    trim: f64,
    /// The latest beat error `steer` found.
    error: Option<Beat>,
    /// No steering until `now` reaches this: a locate was sent and the transport
    /// has not taken it yet.
    held_until: Option<Seconds>,
}

/// The MTC state machine.
#[derive(Debug)]
struct MtcFollower {
    decoder: MtcDecoder,
    sysex: SysexAssembler<[u8; SYSEX_ROOM]>,
    /// The position of the last completed cycle.
    last_cycle: Option<SmpteTimecode>,
    /// Where the source was and when: the last cycle's frame at the arrival of
    /// its piece 0, or a Full Message's position at its arrival.
    origin: Option<Origin>,
    /// The arrival of the last piece 0.
    piece0_at: Option<Seconds>,
    /// Consecutive consistent cycles.
    run: usize,
    locked: bool,
    /// Whether a play was announced and no stop since.
    running: bool,
    /// Whether the source's pieces are descending.
    reversed: bool,
    last_heard: Option<Seconds>,
    /// The latest time error `steer` found.
    error: Option<Seconds>,
    /// No steering until `now` reaches this: a locate was sent and the
    /// transport has not taken it yet.
    held_until: Option<Seconds>,
}

/// A position of the source, as of an arrival.
#[derive(Clone, Copy, Debug)]
struct Origin {
    time: Seconds,
    at: Seconds,
    rate: SmpteFrameRate,
}

/// Longest Universal message read: the Full Message is ten bytes with its
/// delimiters. Room for one open message and one finished.
const SYSEX_MAX: usize = 16;
const SYSEX_ROOM: usize = 2 * SYSEX_MAX + 8;

impl ClockFollower {
    /// The smallest move of the tempo estimate announced again: 0.1 bpm.
    /// Tutti's choice: over a full window the fit's own noise is a hundredth of
    /// a bpm for a millisecond of arrival jitter, so a move this size is the
    /// source's tempo moving; and it is a tenth of a percent at 120 bpm, a beat
    /// of drift in a thousand.
    pub const TEMPO_STEP: Bpm = Bpm::of(0.1);

    /// The time a Beat Clock beat error is closed in: 4 seconds (tutti's
    /// choice). Short enough that a drift of a tenth of a beat is gone in a
    /// phrase, long enough that the trim it asks (`e * 60 / 4` bpm) moves by
    /// [`TEMPO_STEP`](Self::TEMPO_STEP) only for an error of 0.0067 beat.
    pub const CATCH_UP: Seconds = Seconds::new(4.0);

    /// How long a locate is given to land before [`steer`](Self::steer) reads
    /// the error again: one second (tutti's choice), a hundred blocks of 512
    /// frames at 48 kHz.
    pub const SETTLE: Seconds = Seconds::new(1.0);

    /// The beat error past which [`steer`](Self::steer) locates instead of
    /// trimming: a quarter of a beat (tutti's choice), 125 ms at 120 bpm, which a
    /// player hears as a flam and a trim of 2% takes twelve seconds to close.
    pub const SLIP: Beat = Beat::new(0.25);

    /// Consistent Quarter Frame cycles that lock the follower: 3, which is
    /// two intervals of agreement over six frames, twelve to ninety-six
    /// quarter frames by rate (tutti's choice; the spec gives none, and says a
    /// reader needs a full cycle of eight messages to know the time).
    pub const MTC_LOCK_CYCLES: usize = 3;

    /// The time error past which [`steer`](Self::steer) locates an MTC
    /// follower instead of leaving it: 2 SMPTE frames (tutti's choice), one
    /// cycle of eight Quarter Frames, the resolution at which the source names
    /// its position (MTC, Quarter Frame Messages): 83 ms at 24 fps, 67 ms at 30.
    /// A smaller error is the extrapolation between cycles and the placement of a
    /// block, which a locate would only repeat.
    pub const MTC_SLIP_FRAMES: u64 = 2;

    /// How long MTC may be silent before the follower unlocks and stops: a
    /// quarter of a second (tutti's choice), three cycles at 24 frames per
    /// second, the slowest rate, and a hundred times the spacing of its Quarter
    /// Frames.
    pub const MTC_DROPOUT: Seconds = Seconds::new(0.25);

    /// A follower of `kind`, locked to nothing.
    #[must_use]
    pub fn new(kind: ClockKind) -> Self {
        Self(match kind {
            ClockKind::BeatClock => Follower::BeatClock(BeatClockFollower::new()),
            ClockKind::Mtc => Follower::Mtc(MtcFollower::new()),
        })
    }

    /// Which clock it reads.
    #[must_use]
    pub fn kind(&self) -> ClockKind {
        match &self.0 {
            Follower::BeatClock(_) => ClockKind::BeatClock,
            Follower::Mtc(_) => ClockKind::Mtc,
        }
    }

    /// Whether the follower is locked to the source now.
    #[must_use]
    pub fn locked(&self) -> bool {
        match &self.0 {
            Follower::BeatClock(f) => f.locked,
            Follower::Mtc(f) => f.locked,
        }
    }

    /// Takes in one UMP of the source's stream, which arrived at `at`, and
    /// passes each event it makes to `out`, in order. A message the kind does
    /// not read makes none. Allocates nothing.
    pub fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        match &mut self.0 {
            Follower::BeatClock(f) => f.step(ump, at, out),
            Follower::Mtc(f) => f.step(ump, at, out),
        }
    }

    /// Notes that `now` has come, with no message: a source silent for its
    /// dropout is unlocked, and a transport it was rolling is stopped. Call it
    /// once per control tick, with a `now` on the clock of `step`'s `at`.
    pub fn poll(&mut self, now: Seconds, out: &mut impl FnMut(ClockEvent)) {
        match &mut self.0 {
            Follower::BeatClock(f) => f.poll(now, out),
            Follower::Mtc(f) => f.poll(now, out),
        }
    }

    /// Closes the drift between the source's beat and the transport's:
    /// `transport` is the beat the transport is at on `now`, on the clock of
    /// `step`'s `at`, or `None` when the transport is not rolling, which
    /// leaves nothing to steer: the error and the trim are cleared. Call it once
    /// per control tick (rules in the module docs). The trim is announced as a [`ClockEvent::Tempo`] by the
    /// next Timing Clock; an error past [`SLIP`](Self::SLIP) says
    /// [`ClockEvent::Slipped`] and locates now. A follower that is not locked and
    /// playing, or a locate that has not landed, leaves the error unread. Does
    /// nothing for MTC. Allocates nothing.
    pub fn steer(
        &mut self,
        transport: Option<ClockPosition>,
        now: Seconds,
        out: &mut impl FnMut(ClockEvent),
    ) {
        match (&mut self.0, transport) {
            (Follower::BeatClock(f), Some(ClockPosition::Beat(beat))) => {
                f.steer(Some(beat), now, out);
            }
            (Follower::BeatClock(f), _) => f.steer(None, now, out),
            (Follower::Mtc(f), Some(ClockPosition::Time(time))) => f.steer(Some(time), now, out),
            (Follower::Mtc(f), _) => f.steer(None, now, out),
        }
    }

    /// The error [`steer`](Self::steer) found last, the source's position less
    /// the transport's, in beats or in seconds by the kind: positive when the
    /// transport is behind. `None` until it
    /// has read one, and while the follower cannot (see `steer`).
    #[must_use]
    pub fn error(&self) -> Option<ClockOffset> {
        match &self.0 {
            Follower::BeatClock(f) => f.error.map(ClockOffset::Beat),
            Follower::Mtc(f) => f.error.map(ClockOffset::Time),
        }
    }

    /// Forgets the source: unlocked, at beat 0, no tempo, no partial frame.
    pub fn reset(&mut self) {
        match &mut self.0 {
            Follower::BeatClock(f) => f.reset(),
            Follower::Mtc(f) => f.reset(),
        }
    }
}

impl BeatClockFollower {
    fn new() -> Self {
        Self {
            decoder: MidiClockDecoder::new(),
            announced: None,
            ticks_since_announce: 0,
            locked: false,
            last_tick: None,
            trim: 0.0,
            error: None,
            held_until: None,
        }
    }

    /// The most the tempo is trimmed either way, as a fraction of the fit: 2%
    /// (tutti's choice), 2.4 bpm at 120, which closes a beat in 25 seconds at the
    /// most and keeps a wrong error from moving the tempo audibly far.
    const MAX_TRIM: f64 = 0.02;

    /// A locate to `position` as of `at`, which the transport takes as new: no
    /// trim carries over, and the error is not read until it has landed.
    fn locate(&mut self, position: Beat, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        self.trim = 0.0;
        self.error = None;
        self.held_until = Some(at + ClockFollower::SETTLE);
        out(ClockEvent::Locate {
            position: ClockPosition::Beat(position),
            at,
        });
    }

    fn steer(&mut self, transport: Option<Beat>, now: Seconds, out: &mut impl FnMut(ClockEvent)) {
        self.error = None;
        let Some(transport) = transport else {
            self.trim = 0.0;
            return;
        };
        if self.held_until.is_some_and(|until| now < until) {
            return;
        }
        let (true, ClockTransportState::Playing, Some(fit), Some(last), Some(ticked)) = (
            self.locked,
            self.decoder.transport_state(),
            self.decoder.tempo_bpm(),
            self.last_tick,
            self.decoder.last_tick_beat(),
        ) else {
            return;
        };
        // The last tick's beat, and the time since it at the fitted tempo.
        let source = ticked.get() + (now - last).get() * fit.get() / 60.0;
        let error = source - transport.get();
        // A beat that is not a number leaves nothing to steer by.
        if !error.is_finite() {
            self.trim = 0.0;
            return;
        }
        self.error = Some(Beat::new(error));
        if error.abs() > ClockFollower::SLIP.get() {
            out(ClockEvent::Slipped {
                error: ClockOffset::Beat(Beat::new(error)),
            });
            self.locate(Beat::new(source), now, out);
            return;
        }
        // The tempo that closes `error` in `CATCH_UP`, as a fraction of the fit.
        let wanted = error * 60.0 / (ClockFollower::CATCH_UP.get() * fit.get());
        self.trim = wanted.clamp(-Self::MAX_TRIM, Self::MAX_TRIM);
    }

    fn poll(&mut self, now: Seconds, out: &mut impl FnMut(ClockEvent)) {
        if self.decoder.drop_out(now) {
            self.announced = None;
            self.trim = 0.0;
            self.error = None;
            if std::mem::take(&mut self.locked) {
                out(ClockEvent::Unlocked);
            }
            if self.decoder.transport_state() == ClockTransportState::Playing {
                self.decoder.stop_msg();
                out(ClockEvent::Stop { at: now });
            }
        }
    }

    fn reset(&mut self) {
        self.decoder.reset();
        self.announced = None;
        self.ticks_since_announce = 0;
        self.locked = false;
        self.last_tick = None;
        self.trim = 0.0;
        self.error = None;
        self.held_until = None;
    }

    fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        let Ok(Message::System(_, system)) = ump.decode() else {
            return;
        };
        match system {
            System::TimingClock => {
                self.decoder.tick(at);
                // The decoder takes a tick later than the one before it.
                if self.last_tick.is_none_or(|last| at > last) {
                    self.last_tick = Some(at);
                }
                self.after_tick(at, out);
            }
            System::Start => {
                self.decoder.start_msg();
                self.locate(Beat::new(0.0), at, out);
                out(ClockEvent::Play);
            }
            System::Continue => {
                self.decoder.continue_msg();
                self.locate(self.decoder.beat_position(), at, out);
                out(ClockEvent::Play);
            }
            System::Stop => {
                self.decoder.stop_msg();
                self.trim = 0.0;
                self.error = None;
                out(ClockEvent::Stop { at });
            }
            System::SongPosition(position) => {
                self.decoder.song_position_msg(position);
                self.locate(self.decoder.beat_position(), at, out);
            }
            _ => {}
        }
    }

    fn after_tick(&mut self, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        let locked = self.decoder.locked();
        if locked != self.locked {
            self.locked = locked;
            if locked {
                out(ClockEvent::Locked);
            } else {
                self.announced = None;
                self.trim = 0.0;
                self.error = None;
                out(ClockEvent::Unlocked);
            }
        }
        if !locked {
            return;
        }
        self.ticks_since_announce += 1;
        let Some(tempo) = self.decoder.tempo_bpm() else {
            return;
        };
        // The fit, trimmed by what `steer` asked.
        // The fit is 20 to 300 bpm and the trim within `MAX_TRIM`, so the product
        // is a finite positive tempo.
        let tempo = Bpm::new(tempo.get() * (1.0 + self.trim))
            .expect("a fit of 20 to 300 bpm trimmed by at most 2% is a tempo");
        let due = match self.announced {
            None => true,
            Some(last) => {
                self.ticks_since_announce >= MidiClockDecoder::LOCK_TICKS
                    && tempo.differs_from(last, ClockFollower::TEMPO_STEP)
            }
        };
        if due {
            self.announced = Some(tempo);
            self.ticks_since_announce = 0;
            out(ClockEvent::Tempo { tempo, at });
        }
    }
}

impl MtcFollower {
    fn new() -> Self {
        Self {
            decoder: MtcDecoder::new(),
            sysex: SysexAssembler::new([0; SYSEX_ROOM], SYSEX_MAX, 1),
            last_cycle: None,
            origin: None,
            piece0_at: None,
            run: 0,
            locked: false,
            running: false,
            reversed: false,
            last_heard: None,
            error: None,
            held_until: None,
        }
    }

    fn poll(&mut self, now: Seconds, out: &mut impl FnMut(ClockEvent)) {
        let silent = self
            .last_heard
            .is_some_and(|at| now - at > ClockFollower::MTC_DROPOUT);
        if silent {
            if std::mem::take(&mut self.running) {
                out(ClockEvent::Stop { at: now });
            }
            if std::mem::take(&mut self.locked) {
                out(ClockEvent::Unlocked);
            }
            self.forget_position();
        }
    }

    fn reset(&mut self) {
        self.locked = false;
        self.running = false;
        self.forget_position();
    }

    fn forget_position(&mut self) {
        self.decoder.reset();
        self.sysex.reset();
        self.last_cycle = None;
        self.origin = None;
        self.piece0_at = None;
        self.run = 0;
        self.reversed = false;
        self.last_heard = None;
        self.error = None;
        self.held_until = None;
    }

    /// A locate to `time` as of `at`, which the transport takes as new: the
    /// error is not read until it has landed.
    fn locate(&mut self, time: Seconds, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        self.error = None;
        self.held_until = Some(at + ClockFollower::SETTLE);
        out(ClockEvent::Locate {
            position: ClockPosition::Time(time),
            at,
        });
    }

    fn steer(
        &mut self,
        transport: Option<Seconds>,
        now: Seconds,
        out: &mut impl FnMut(ClockEvent),
    ) {
        self.error = None;
        let Some(transport) = transport else {
            return;
        };
        if self.held_until.is_some_and(|until| now < until) {
            return;
        }
        let (true, true, Some(origin)) = (self.locked, self.running, self.origin) else {
            return;
        };
        // The source's time at `now`: the cycle's frame, and the time since its
        // piece 0 at the nominal rate. The source is taken to run at the rate
        // it names; a source off that speed shows as an error that grows.
        let source = origin.time.get() + (now - origin.at).get();
        let error = source - transport.get();
        // A time that is not a number leaves nothing to steer by.
        if !error.is_finite() {
            return;
        }
        self.error = Some(Seconds::new(error));
        let frames = origin.rate.frames_at(Seconds::new(error)).abs();
        if frames > TimecodeFrames::from_count(ClockFollower::MTC_SLIP_FRAMES) {
            out(ClockEvent::Slipped {
                error: ClockOffset::Time(Seconds::new(error)),
            });
            self.locate(Seconds::new(source), now, out);
        }
    }

    /// A source running backward: report it once, and stop what the transport
    /// was doing for the source.
    fn reverse(&mut self, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        self.reversed = true;
        out(ClockEvent::Reversed);
        if std::mem::take(&mut self.running) {
            out(ClockEvent::Stop { at });
        }
        if std::mem::take(&mut self.locked) {
            out(ClockEvent::Unlocked);
        }
        self.last_cycle = None;
        self.origin = None;
        self.run = 0;
        self.error = None;
    }

    fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        self.sysex.begin_block();
        if let SysexStep::Complete(done) = self.sysex.push(ump) {
            if let UniversalRead::Universal(Addressed {
                message: Universal::MtcFullMessage(full),
                ..
            }) = read(self.sysex.message(done))
            {
                self.last_heard = Some(at);
                // The partial cycle and the last one belong to the old position.
                self.decoder.reset();
                self.last_cycle = None;
                self.run = 0;
                self.reversed = false;
                let time = full.time_code().timecode();
                self.origin = Some(Origin {
                    time: time.to_seconds(),
                    at,
                    rate: time.frame_rate(),
                });
                self.locate(time.to_seconds(), at, out);
            }
            return;
        }
        if !self.decoder.feed(&ump) {
            return;
        }
        self.last_heard = Some(at);
        if self.decoder.last_piece() == Some(0) {
            self.piece0_at = Some(at);
        }
        match self.decoder.direction() {
            Some(MtcDirection::Reverse) if !self.reversed => self.reverse(at, out),
            Some(MtcDirection::Forward) if self.reversed => self.reversed = false,
            _ => {}
        }
        let Some(cycle) = self.decoder.take_cycle() else {
            return;
        };
        if self.reversed || cycle.direction == MtcDirection::Reverse {
            return;
        }
        // Piece 0 began the cycle (a cycle is only assembled from one), so its
        // arrival is when `frame` started.
        self.cycle(cycle.frame, self.piece0_at.unwrap_or(at), out);
    }

    /// A forward cycle: `frame` started at `origin`, the arrival of its piece 0.
    /// The source is two frames on by the time the last piece arrives (MTC,
    /// Quarter Frame Message Implementation), and a locate says where it was
    /// at `origin`, so the transport that applies it later advances by the
    /// difference as for any locate.
    fn cycle(&mut self, frame: SmpteTimecode, origin: Seconds, out: &mut impl FnMut(ClockEvent)) {
        // Whole cycles on: one after the last, or more when the assembler
        // discarded a cycle between (a gap costs a cycle, not the lock), up to
        // as many as `MTC_DROPOUT` holds.
        let consistent = self.last_cycle.is_some_and(|last| {
            let step = frame.frame_count().checked_sub(last.frame_count());
            last.frame_rate() == frame.frame_rate()
                && step.is_some_and(|step| {
                    step >= CYCLE_FRAMES
                        && step % CYCLE_FRAMES == 0
                        && frame.frame_rate().frames_at(ClockFollower::MTC_DROPOUT)
                            >= TimecodeFrames::from_count(step)
                })
        });
        let first = self.last_cycle.is_none();
        self.last_cycle = Some(frame);
        let time = frame.to_seconds();
        self.origin = Some(Origin {
            time,
            at: origin,
            rate: frame.frame_rate(),
        });
        if consistent {
            self.run += 1;
        } else {
            self.run = 1;
            if self.running && !first {
                self.locate(time, origin, out);
            }
        }
        if self.run >= ClockFollower::MTC_LOCK_CYCLES {
            if !self.locked {
                self.locked = true;
                out(ClockEvent::Locked);
            }
            if !self.running {
                self.running = true;
                self.locate(time, origin, out);
                out(ClockEvent::Play);
            }
        }
    }
}
