//! [`TransportHandle`]: the transport's verbs from any thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tutti_engine::BlockClockReader;
use tutti_rt::Mutex;
use tutti_timeline::{
    FadeOut, LoopRegion, MotionCommand, RECORDING_CAPACITY, Recording, Roll, TakeId, TimedError,
    Transport,
};
use tutti_units::{At, Beat, Frame};

use crate::Playhead;

/// A session's transport verbs, owned by no thread: `play`, `stop`, `locate`
/// and `schedule` as [`Session`](crate::Session) has them, from a handle that
/// is `Send` and `Clone`.
///
/// Every clone sends into the session's one motion queue, and every clone
/// shares the control side's record of whether the last motion sent left the
/// transport rolling, so [`TimedError::BeatWhileStopped`] is refused on any
/// thread as on the session. With senders on several threads at once, the
/// record follows the order their sends updated it, which may differ from the
/// order the queue holds them. Take one with
/// [`Session::transport_handle`](crate::Session::transport_handle).
///
/// A handle holds no borrow of its session: once the session is dropped, its
/// sends go into a queue nothing drains, and succeed until that queue is full.
#[derive(Clone, Debug)]
pub struct TransportHandle {
    transport: Transport,
    /// The engine's block clock; `None` detached, where nothing renders.
    clock: Option<BlockClockReader>,
    /// The last motion sent for the next block that starts or stops the
    /// transport, and the frame the next render call was to start at when it
    /// was sent ([`Sent`]). It speaks for the transport until a call renders
    /// past that frame; from then on the playhead does.
    sent: Arc<AtomicU64>,
    /// The takes that may be recording, shared by every clone: the control
    /// side's superset of the engine's set ([`OpenTakes`]).
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "a take (feature `capture`) is the one sender")
    )]
    open: Arc<Mutex<OpenTakes>>,
}

/// When a take's [`Recording`] command is due: a frame, or the next block. A
/// beat has no frame on the control side, so a take resolves its beat first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "capture"),
    expect(dead_code, reason = "a take (feature `capture`) is the one sender")
)]
pub(crate) enum When {
    NextBlock,
    Frame(Frame),
}

impl TransportHandle {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            transport,
            clock: None,
            sent: Arc::new(AtomicU64::new(Sent::NONE)),
            open: Arc::default(),
        }
    }

    pub(crate) fn set_clock(&mut self, clock: BlockClockReader) {
        self.clock = Some(clock);
    }

    /// The project's tempo, a target of [`Session::set`](crate::Session::set)
    /// and [`Session::get`](crate::Session::get).
    #[must_use]
    pub const fn tempo(&self) -> crate::TempoTarget {
        crate::TempoTarget::new()
    }

    /// The project's meter, a target of [`Session::set`](crate::Session::set)
    /// and [`Session::get`](crate::Session::get).
    #[must_use]
    pub const fn meter(&self) -> crate::MeterTarget {
        crate::MeterTarget::new()
    }

    /// The loop region, a target of [`Session::set`](crate::Session::set)
    /// and [`Session::get`](crate::Session::get).
    #[must_use]
    pub const fn looping(&self) -> crate::LoopTarget {
        crate::LoopTarget::new()
    }

    /// The playhead as the engine's last block wrote it; the default while no
    /// engine renders.
    pub(crate) fn playhead(&self) -> Playhead {
        self.clock
            .as_ref()
            .map_or_else(Playhead::default, BlockClockReader::playhead)
    }

    /// Starts the transport rolling from where it stands, at the next block.
    /// A no-op while rolling.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the transport's command queue is full; the
    /// event is handed back in it.
    pub fn play(&self) -> Result<(), TimedError<MotionCommand>> {
        self.schedule(At::NextBlock, MotionCommand::Play)
    }

    /// Stops the transport where it stands, at the next block, after the
    /// [`FadeOut`] it names.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn stop(&self, fade: FadeOut) -> Result<(), TimedError<MotionCommand>> {
        self.schedule(At::NextBlock, MotionCommand::Stop { fade })
    }

    /// Jumps to `beat` at the next block, keeping whatever the transport was
    /// doing (a locate while rolling keeps rolling), with a declick fade.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn locate(&self, beat: Beat) -> Result<(), TimedError<MotionCommand>> {
        self.schedule(At::NextBlock, MotionCommand::locate(beat))
    }

    /// Sends a motion `event` to take effect at `at`: a frame, a beat, or the
    /// next block (`At`, or a `Beat` or `Frame` as it stands). The one timed
    /// verb: `schedule(b, MotionCommand::Play)` is play at beat `b`, and every
    /// [`MotionCommand`] (a locate that then rolls, a stop that returns to a
    /// beat) is timed the same way. The untimed verbs are `At::NextBlock`.
    ///
    /// A beat is reached by playback: it has a frame only while the transport
    /// rolls. A command that starts the transport rolling ([`MotionCommand::Play`],
    /// or a locate that then rolls) timed at a beat while the transport is
    /// stopped could only fire after the playback it starts, so it is refused
    /// where it is made. Stopped is the control side's word: the last start or
    /// stop sent for the next block through the session or any handle (a
    /// `play` just before is rolling), until a render call passes it; after
    /// that, the playhead (which also sees a play timed at a frame fire). A command that does
    /// not start playback (a stop, a plain locate) waits for the playback to
    /// reach its beat, as any beat command does.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the timed-command list is full, and
    /// [`TimedError::BeatWhileStopped`] for the command above.
    pub fn schedule(
        &self,
        at: impl Into<At>,
        event: MotionCommand,
    ) -> Result<(), TimedError<MotionCommand>> {
        let at = at.into();
        let roll = event.roll();
        if let At::Beat(_) = at
            && roll == Roll::Starts
            && !self.rolling()
        {
            return Err(TimedError::BeatWhileStopped(event));
        }
        self.transport.motion().schedule(at, event)?;
        if at == At::NextBlock {
            match roll {
                Roll::Starts => self.record(true),
                Roll::Stops => self.record(false),
                Roll::Keeps => {}
            }
        }
        Ok(())
    }

    /// Sends a take's [`Recording`] command, keeping the control side's set of
    /// open takes ([`OpenTakes`]). Only a take sends one: an app records
    /// through [`Session::record`](crate::Session::record).
    ///
    /// A `Start` for a take already open sends nothing and succeeds. A `Stop`
    /// for a take that is not open sends nothing and changes nothing; one for
    /// an open take that would not end its recording sooner than the `Stop`
    /// already sent sends nothing either.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] with the command when [`RECORDING_CAPACITY`] takes
    /// are open (none with a `Stop` the engine has rendered past), or when the
    /// transport's command queue is full.
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "a take (feature `capture`) is the one sender")
    )]
    pub(crate) fn send_recording(
        &self,
        recording: Recording,
        when: When,
    ) -> Result<(), TimedError<Recording>> {
        let next = Sent::next_frame(&self.playhead());
        let at = match when {
            When::NextBlock => At::NextBlock,
            When::Frame(frame) => At::Frame(frame),
        };
        let mut open = self.open.lock();
        match recording {
            Recording::Start(id) => {
                open.free_stopped(next);
                if open.contains(id) {
                    return Ok(());
                }
                if open.is_full() {
                    return Err(TimedError::Full(recording));
                }
                self.transport.motion().schedule(at, recording)?;
                open.insert(id);
            }
            Recording::Stop(id) => {
                // A frame already past is applied at the next render's first
                // frame: the engine's set drops the take then, not before.
                let ends = match when {
                    When::NextBlock => next,
                    When::Frame(frame) => frame.get().max(next),
                };
                if !open.ends_sooner(id, ends) {
                    return Ok(());
                }
                self.transport.motion().schedule(at, recording)?;
                open.stop(id, ends);
            }
        }
        Ok(())
    }

    /// Whether the transport rolls as far as the control side knows: the
    /// last next-block start or stop sent, until a render call passes it;
    /// then what the engine last rendered, which also covers a play timed at
    /// a frame.
    fn rolling(&self) -> bool {
        let playhead = self.playhead();
        match Sent::decode(self.sent.load(Ordering::Relaxed)) {
            Some((next, rolling)) if next == Sent::next_frame(&playhead) => rolling,
            _ => playhead.playing(),
        }
    }

    /// Records a next-block start (`true`) or stop (`false`) against the
    /// frame the next render call starts at.
    fn record(&self, rolling: bool) {
        let next = Sent::next_frame(&self.playhead());
        self.sent
            .store(Sent::encode(next, rolling), Ordering::Relaxed);
    }

    /// The loop region the engine last published: its bounds and whether it is
    /// armed.
    #[must_use]
    pub fn loop_region(&self) -> LoopRegion {
        self.transport.motion().loop_region()
    }

    /// Drops every timed command not yet fired, from the next block, but a
    /// take's recording command (a take's start and stop are never lost). The
    /// rolling record is unchanged: only timed commands are cancelled, and
    /// the record holds only next-block commands.
    pub fn cancel_scheduled(&self) {
        self.transport.motion().cancel_scheduled();
    }

    /// How many timed commands are waiting to fire.
    #[must_use]
    pub fn scheduled_outstanding(&self) -> usize {
        self.transport.motion().scheduled_outstanding()
    }
}

/// The packing of [`TransportHandle`]'s record into one word: the frame the
/// next render call was to start at, shifted left by one, with the rolling
/// bit below it; [`Sent::NONE`] before any start or stop is sent.
struct Sent;

impl Sent {
    const NONE: u64 = u64::MAX;

    /// The frame the next render call starts at: past the last call's span.
    /// Zero before any call renders, and greater after each call that renders
    /// frames.
    fn next_frame(playhead: &Playhead) -> u64 {
        (playhead.frame() + playhead.span()).get()
    }

    fn encode(next: u64, rolling: bool) -> u64 {
        (next << 1) | u64::from(rolling)
    }

    fn decode(word: u64) -> Option<(u64, bool)> {
        (word != Self::NONE).then_some((word >> 1, word & 1 == 1))
    }
}

/// The takes that may be recording, as the control side knows them: at most
/// [`RECORDING_CAPACITY`], each with the frame its `Stop` takes effect, `None`
/// until one is sent. The engine's set of takes recording is a subset of this
/// one at every instant, so it never overflows.
#[derive(Debug, Default)]
#[cfg_attr(
    not(feature = "capture"),
    expect(dead_code, reason = "a take (feature `capture`) is the one sender")
)]
struct OpenTakes {
    takes: Vec<(TakeId, Option<u64>)>,
}

#[cfg_attr(
    not(feature = "capture"),
    expect(dead_code, reason = "a take (feature `capture`) is the one sender")
)]
impl OpenTakes {
    /// Forgets the takes whose `Stop` the engine has rendered past: its frame
    /// is before `next`, the first frame of the next render.
    fn free_stopped(&mut self, next: u64) {
        self.takes
            .retain(|&(_, ends)| ends.is_none_or(|ends| ends >= next));
    }

    fn contains(&self, id: TakeId) -> bool {
        self.takes.iter().any(|&(open, _)| open == id)
    }

    fn is_full(&self) -> bool {
        self.takes.len() >= RECORDING_CAPACITY
    }

    fn insert(&mut self, id: TakeId) {
        self.takes.push((id, None));
    }

    /// Whether a `Stop` taking effect at `ends` ends `id` sooner than any sent
    /// before: `false` for a take not open.
    fn ends_sooner(&self, id: TakeId, ends: u64) -> bool {
        self.takes
            .iter()
            .any(|&(open, was)| open == id && was.is_none_or(|was| ends < was))
    }

    fn stop(&mut self, id: TakeId, ends: u64) {
        for (open, was) in &mut self.takes {
            if *open == id {
                *was = Some(ends);
            }
        }
    }
}
