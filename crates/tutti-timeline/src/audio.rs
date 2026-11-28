//! The audio-thread side of a transport, taken once.
//!
//! [`Transport::audio`] hands out a [`TransportAudio`]: the motion machine,
//! the playhead clock, the timed commands and the untimed inputs a block
//! reads, as one value owned by whoever renders under the transport. Its
//! methods are the block's steps in the order they are legal, and everything
//! they work on is private, so the engine cannot read the inputs twice,
//! settle a fade late, free a command's credit by hand or hold the playhead
//! writer apart from the machine.

use super::clock::{Control, TransportClock};
use super::handle::Transport;
use super::motion::MotionAudio;
use super::settings::TransportSettings;
use super::tempo_reader::TempoReader;
use super::timed::{CommandQueueAudio, Recording, Scheduled, TransportCommand};
use super::walk::UpcomingTempo;
use std::sync::atomic::Ordering;
use tutti_music::MeterMap;
use tutti_rt::{Loan, Reader};
use tutti_tempo::TempoMap;
use tutti_units::{Beat, BeatDuration, Bpm, SampleRate, Seconds};

/// [`Transport::audio`] refused: another [`TransportAudio`] over this
/// transport (or a clone of it) is alive. Drop it (the engine that holds it)
/// before building another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the transport's audio side is held by another engine")]
pub struct AudioTaken;

/// What applying a command did to the output: the transport has moved at once
/// either way, and a declicked stop or seek leaves the fade to the caller,
/// as gain, on the frame the command landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[must_use]
pub enum Applied {
    /// No fade: the output continues.
    Plain,
    /// A declicked stop or seek landed: the fade-out ends, and the fade-in
    /// starts, here.
    Declicked,
}

impl Applied {
    /// Whether a declicked stop or seek landed.
    #[must_use]
    pub fn declicked(self) -> bool {
        self == Self::Declicked
    }
}

/// The audio thread's transport: see the module docs.
///
/// Not `Clone`. Dropping it hands the motion machine (with the loop region
/// and the claim on the playhead) back to the [`Transport`], so a later
/// engine resumes the same state. What it is built from stays out of reach,
/// so a second audio side cannot be assembled around it:
///
/// ```compile_fail,E0624
/// let t = tutti_timeline::Transport::new(48_000.0);
/// let _ = t.motion().audio();
/// ```
///
/// ```compile_fail,E0277
/// fn is_clone<T: Clone>() {}
/// is_clone::<tutti_timeline::TransportAudio>();
/// ```
pub struct TransportAudio {
    motion: MotionAudio,
    clock: TransportClock,
    /// The untimed inputs, read at a block's start and carried across the
    /// commands in it.
    control: Control,
    schedule: CommandQueueAudio,
    settings: TransportSettings,
    /// The published tempo map, lent only outside a segment's one loan: by
    /// `take` and by `with_lent_map` (a rate change, outside any walk). The
    /// walk and everything else in a segment read the caller's loan
    /// ([`tempo_reader`](Self::tempo_reader)).
    tempo_map: TempoReader,
    /// The publisher's count at the last block that took the map.
    seen: u64,
    /// The project's meter, lent once per rendered block
    /// ([`meter`](Self::meter)). Registered here, on the control side, when
    /// the audio side is taken.
    meter: Reader<MeterMap>,
    /// Whether the map in force changes tempo: the clock then follows it,
    /// lending it for each step that needs it and
    /// dropping the loan at the end of the step. A constant map leaves this
    /// off and costs nothing.
    stepped: bool,
}

impl std::fmt::Debug for TransportAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportAudio").finish_non_exhaustive()
    }
}

/// The commands still waiting go back first, then the machine: the next
/// audio side can only be taken once the machine is home, and finds the
/// list there.
impl Drop for TransportAudio {
    fn drop(&mut self) {
        self.schedule.hand_back();
        self.motion.hand_back();
    }
}

impl TransportAudio {
    pub(crate) fn take(transport: &Transport, rate: SampleRate) -> Result<Self, AudioTaken> {
        let motion = transport.motion.audio().ok_or(AudioTaken)?;
        let settings = transport.settings.clone();
        let loop_region = motion.loop_region();
        // The count first, then the tempo: a publish between the two is taken
        // at the first block, never missed.
        let seen = settings.map_generation.load(Ordering::Acquire);
        let mut tempo_map = settings.tempo_reader();
        // A map with steps published before this audio side was taken (an
        // engine built after the project's tempo was set, or rebuilt by a
        // device restart) is followed from the first block, as a later publish
        // would be.
        let (clock, tempo, stepped) = {
            let map = tempo_map.lend();
            let tempo = map.tempo_at(Beat::new(0.0));
            let mut clock = TransportClock::new(transport.links(), rate, tempo);
            let stepped = map.constant_tempo().is_none();
            if stepped {
                clock.follow(&map);
            }
            (clock, tempo, stepped)
        };
        Ok(Self {
            clock,
            control: Control::read(&settings, loop_region, motion.recording(), tempo),
            tempo_map,
            seen,
            meter: settings.meter_reader(),
            stepped,
            schedule: transport.motion.queue_audio(),
            motion,
            settings,
        })
    }

    /// Moves the commands the control thread sent
    /// ([`schedule`](super::MotionFsm::schedule), and
    /// [`send`](super::MotionFsm::send), which is `At::NextBlock`)
    /// into the pending list, dropping what it cancelled. Call at the start
    /// of each walk.
    pub fn drain_commands(&mut self) {
        self.schedule.drain_commands();
    }

    /// The pending command that comes first, as of the last
    /// [`drain_commands`](Self::drain_commands): `due` answers `Some((k, late))`
    /// for a command due `k` frames into the block (or `late`) and `None` for
    /// one not due in it. The least `k` wins, send order breaking ties.
    /// Returns `(k, index, late)`, the index
    /// [`apply_command`](Self::apply_command) takes.
    pub fn earliest_due(
        &self,
        due: impl FnMut(&Scheduled) -> Option<(usize, bool)>,
    ) -> Option<(usize, usize, bool)> {
        self.schedule.earliest(due)
    }

    /// The least of what `read` makes of each pending command that it reads
    /// one off, as of the last [`drain_commands`](Self::drain_commands).
    pub fn least_pending<T: Ord>(&self, read: impl FnMut(&Scheduled) -> Option<T>) -> Option<T> {
        self.schedule.least(read)
    }

    /// The scheduled commands sent and not yet applied.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> &[Scheduled] {
        self.schedule.pending()
    }

    /// The count of tempo-map publishes so far. Read it **before** lending
    /// the map the block runs on ([`tempo_reader`](Self::tempo_reader)) and
    /// pass both to [`begin_block`](Self::begin_block): a publish between the
    /// two is then taken at the next block, never missed.
    #[must_use]
    pub fn tempo_generation(&self) -> u64 {
        self.settings.map_generation.load(Ordering::Acquire)
    }

    /// A reader of the map the block runs on, for the caller that lends it
    /// once per block and hands the one loan to every call below and to the
    /// graph's render (`Executor::render_applied`'s `tempo`), so the block's
    /// position, its cuts and the nodes' `Cx::tempo` all come from one map.
    /// Registers a reader: control thread, once.
    #[must_use]
    pub fn tempo_reader(&self) -> TempoReader {
        self.settings.tempo_reader()
    }

    /// Reads the untimed inputs for a block and reports the transport at its
    /// first frame. Once per block: a store from the control thread lands at
    /// the next block, never at a cut in this one. `map` is the block's map
    /// and `generation` the count read before it was lent
    /// ([`tempo_generation`](Self::tempo_generation)); the calls after this
    /// one in the block take the same `map`.
    pub fn begin_block(
        &mut self,
        generation: u64,
        map: &TempoMap,
    ) -> tutti_graph::TransportPosition {
        let tempo = self.tempo_for_block(generation, map);
        self.control = Control::read(
            &self.settings,
            self.motion.loop_region(),
            self.motion.recording(),
            tempo,
        );
        self.begin(true, map)
    }

    /// The tempo this block renders at: the published map's when a publish
    /// landed since the last block, else the one in force.
    ///
    /// A map that changes tempo is taken here too: the clock re-seats on it at
    /// the current beat, and from then on follows it, the
    /// tempo of the block being the walk's and not this scalar.
    fn tempo_for_block(&mut self, published: u64, map: &TempoMap) -> Bpm {
        if published != self.seen {
            self.seen = published;
            match map.constant_tempo() {
                Some(tempo) => {
                    self.stepped = false;
                    return tempo;
                }
                None => {
                    self.stepped = true;
                    self.clock.follow(map);
                }
            }
        }
        self.control.tempo
    }

    /// Runs `f` on the clock with `map` when it changes tempo.
    fn with_map<R>(
        &mut self,
        map: &TempoMap,
        f: impl FnOnce(&mut TransportClock, Option<&TempoMap>) -> R,
    ) -> R {
        let stepped = self.stepped.then_some(map);
        f(&mut self.clock, stepped)
    }

    /// Runs `f` on the clock with the map in force, lent for the call and no
    /// longer: for the calls made outside a block's walk.
    fn with_lent_map<R>(
        &mut self,
        f: impl FnOnce(&mut TransportClock, Option<&TempoMap>) -> R,
    ) -> R {
        if self.stepped {
            let map = self.tempo_map.lend();
            f(&mut self.clock, Some(&map))
        } else {
            f(&mut self.clock, None)
        }
    }

    fn begin(&mut self, take_seek: bool, map: &TempoMap) -> tutti_graph::TransportPosition {
        let control = self.control;
        self.with_map(map, |clock, map| clock.begin(&control, take_seek, map))
    }

    /// The project's meter, lent for one rendered piece and handed to the
    /// graph with it (`Executor::render_applied`). Wait-free; drop the loan
    /// at the piece's end, never keep it across blocks.
    #[must_use = "a loan dropped at once lends nothing"]
    pub fn meter(&mut self) -> Loan<'_, MeterMap> {
        self.meter.lend()
    }

    /// The tempo changes inside the next `frames` of the block that began
    /// under `from`, and where the block must end if one does not fit
    /// `free` is how many changes the block can still
    /// take: `MAX_TRANSPORT_CHANGES` less those already cut. Moves nothing.
    ///
    /// Nothing for a constant map (the common case: no lend, no walk) and for
    /// a stopped transport. The changes are offsets from the clock's current
    /// frame, so a caller that has cut the block calls this again from its
    /// cut.
    pub fn tempo_ahead(
        &mut self,
        frames: usize,
        from: &tutti_graph::TransportPosition,
        free: usize,
        map: &TempoMap,
    ) -> UpcomingTempo {
        if !self.stepped {
            return UpcomingTempo::NONE;
        }
        self.with_map(map, |clock, map| clock.ahead(frames, from, map, free))
    }

    /// The transport at the clock's current frame, after
    /// [`advance`](Self::advance) stopped at a tempo change from
    /// [`tempo_ahead`](Self::tempo_ahead): the tempo and segment the walk
    /// took there. The inputs read at the block's start stay.
    pub fn position_after_tempo_change(
        &mut self,
        map: &TempoMap,
    ) -> tutti_graph::TransportPosition {
        self.begin(false, map)
    }

    /// Applies pending command `i` (late or not: counted when late), then
    /// reports the transport from this frame on under it. The command's own
    /// effect only; the inputs read at the block's start stay.
    pub fn apply_command(
        &mut self,
        i: usize,
        late: bool,
        map: &TempoMap,
    ) -> (Applied, tutti_graph::TransportPosition) {
        let cmd = self.schedule.take(i, late).command();
        self.motion.apply(cmd);
        if let TransportCommand::Recording(Recording::Stop(id)) = cmd {
            self.schedule.drop_starts(id);
        }
        let applied = self.settle();
        self.control
            .apply(&cmd, &self.settings, self.motion.recording());
        // Only a motion command can have requested a seek.
        let seeks = matches!(cmd, TransportCommand::Motion(_));
        (applied, self.begin(seeks, map))
    }

    /// Advances the playhead `frames` under `from`, the transport the last
    /// [`begin_block`](Self::begin_block) or [`apply_command`](Self::apply_command) reported.
    pub fn advance(
        &mut self,
        frames: usize,
        from: &tutti_graph::TransportPosition,
        map: &TempoMap,
    ) {
        self.with_map(map, |clock, map| clock.advance(frames, from, map));
    }

    /// The transport time of `beat`, in seconds on the project's tempo map: a
    /// constant map at `tempo` (the tempo of the block that stands at
    /// `beat`), a stepped one by its own segments, read on `map`, the
    /// block's one loan. Audio thread: no allocation.
    pub fn seconds_at(&self, beat: Beat, tempo: Bpm, map: &TempoMap) -> Seconds {
        if self.stepped {
            map.seconds_at(beat)
        } else {
            BeatDuration::new(beat.get()).to_seconds(tempo)
        }
    }

    /// Publishes the playhead after the block it belongs to is rendered.
    pub fn publish_position(&self) {
        self.clock.publish_position();
    }

    /// Follows the executor's rate: a new segment at `rate`, and the timed
    /// commands written in frames move to the same wall-clock time. A no-op
    /// at the rate the clock already runs at.
    pub fn follow_rate(&mut self, rate: SampleRate) {
        let was = self.clock.sample_rate();
        if rate != was {
            self.with_lent_map(|clock, map| clock.set_sample_rate(rate, map));
            self.schedule.rescale(rate.get() / was.get());
        }
    }

    /// Jumps the published playhead to `beat`, for a host that drives no
    /// engine (a headless tool, a test): a jump to a reader, so the segment
    /// generation moves on first.
    ///
    /// ```
    /// use tutti_units::{Beat, SampleRate};
    /// use tutti_timeline::{Timeline, Transport};
    /// let transport = Transport::new(SampleRate::SR_48K);
    /// let mut audio = transport.audio(SampleRate::SR_48K).expect("the only audio side");
    /// audio.set_playhead(Beat::new(4.0));
    /// assert_eq!(transport.beat(), Beat::new(4.0));
    /// // A jump: a reader keyed on the segment re-seats.
    /// assert_eq!(transport.segment_generation(), 1);
    /// // The audio side is held: nobody else may store.
    /// assert!(transport.audio(SampleRate::SR_48K).is_err());
    /// ```
    pub fn set_playhead(&mut self, beat: impl Into<Beat>) {
        self.clock.set_playhead(beat.into());
    }

    /// A declicked stop or seek parks the machine in a fade state; the fade
    /// is the caller's, as gain, so settle the machine now.
    fn settle(&mut self) -> Applied {
        if self.motion.complete_declick() {
            Applied::Declicked
        } else {
            Applied::Plain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MotionCommand, Recording, TakeId, Timeline};
    use tutti_units::{At, Frame};

    use tutti_graph::TransportPosition;

    /// The block calls with a map lent for each call: a test's block is one
    /// call, so the map it lends is the one the call reads.
    trait Lend {
        fn lend_begin_block(&mut self) -> TransportPosition;
        fn lend_tempo_ahead(
            &mut self,
            frames: usize,
            from: &TransportPosition,
            free: usize,
        ) -> UpcomingTempo;
        fn lend_apply_command(&mut self, i: usize, late: bool) -> (Applied, TransportPosition);
        fn lend_advance(&mut self, frames: usize, from: &TransportPosition);
    }

    impl Lend for TransportAudio {
        fn lend_begin_block(&mut self) -> TransportPosition {
            let generation = self.tempo_generation();
            let mut reader = self.tempo_reader();
            let map = reader.lend();
            self.begin_block(generation, &map)
        }
        fn lend_tempo_ahead(
            &mut self,
            frames: usize,
            from: &TransportPosition,
            free: usize,
        ) -> UpcomingTempo {
            let mut reader = self.tempo_reader();
            let map = reader.lend();
            self.tempo_ahead(frames, from, free, &map)
        }
        fn lend_apply_command(&mut self, i: usize, late: bool) -> (Applied, TransportPosition) {
            let mut reader = self.tempo_reader();
            let map = reader.lend();
            self.apply_command(i, late, &map)
        }
        fn lend_advance(&mut self, frames: usize, from: &TransportPosition) {
            let mut reader = self.tempo_reader();
            let map = reader.lend();
            self.advance(frames, from, &map);
        }
    }

    fn constant(bpm: f64) -> std::sync::Arc<TempoMap> {
        std::sync::Arc::new(TempoMap::constant(Bpm::of(bpm)).expect("a tempo"))
    }

    /// One audio side per transport, through any clone, and it comes back
    /// with the machine when it drops.
    #[test]
    fn the_audio_side_is_taken_once_and_comes_back() {
        let t = Transport::new(SampleRate::SR_48K);
        let first = t.audio(SampleRate::SR_48K).expect("first");
        assert_eq!(t.clone().audio(SampleRate::SR_48K).err(), Some(AudioTaken));
        drop(first);
        t.audio(SampleRate::SR_48K).expect("handed back on drop");
    }

    /// A command still waiting for its beat outlives the audio side that
    /// gathered it, credit and all: the next one (a device restart) finds it
    /// pending, fires it, and the credits return to zero.
    #[test]
    fn a_waiting_command_outlives_its_audio_side() {
        let t = Transport::new(SampleRate::SR_48K);
        t.motion
            .schedule(
                At::Beat(tutti_units::Beat::new(1000.0)),
                MotionCommand::Play,
            )
            .expect("room");
        let mut first = t.audio(SampleRate::SR_48K).expect("first");
        first.drain_commands();
        assert_eq!(first.pending().len(), 1);
        drop(first);

        let mut next = t.audio(SampleRate::SR_48K).expect("handed back on drop");
        next.drain_commands();
        assert_eq!(next.pending().len(), 1, "carried over");
        assert_eq!(t.motion.scheduled_outstanding(), 1);
        assert!(!t.is_rolling());
        let _ = next.lend_apply_command(0, false);
        assert_eq!(t.motion.scheduled_outstanding(), 0);
        assert!(t.is_rolling(), "the carried command fired");
    }

    /// Firing a command gives its credit back and counts it late exactly
    /// when it is; nothing else frees a credit.
    #[test]
    fn firing_a_command_frees_its_credit_and_counts_it_late() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut a = t.audio(SampleRate::SR_48K).expect("first");
        t.motion
            .schedule(At::Frame(Frame::new(0)), MotionCommand::Play)
            .expect("room");
        t.motion
            .schedule(At::NextBlock, MotionCommand::stop_unfaded())
            .expect("room");
        a.drain_commands();
        assert_eq!(a.pending().len(), 2);
        assert_eq!(t.motion.scheduled_outstanding(), 2);
        let _ = a.lend_apply_command(0, true);
        assert_eq!(t.motion.scheduled_outstanding(), 1);
        assert_eq!(t.motion.late_command_count(), 1);
        let _ = a.lend_apply_command(0, false);
        assert_eq!(t.motion.scheduled_outstanding(), 0);
        assert_eq!(t.motion.late_command_count(), 1);
        assert!(!t.is_rolling(), "play then stop");
    }

    /// The block renders at the published map's tempo, from the first block
    /// after the publish, and stays there.
    #[test]
    fn a_published_constant_map_is_the_next_blocks_tempo() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut a = t.audio(SampleRate::SR_48K).expect("first");
        assert_eq!(a.lend_begin_block().tempo, Bpm::of(120.0));
        t.settings.set_tempo_map(constant(97.25));
        assert_eq!(a.lend_begin_block().tempo, Bpm::of(97.25));
        assert_eq!(a.lend_begin_block().tempo, Bpm::of(97.25), "and stays");
        t.settings.set_tempo(Bpm::of(143.5));
        assert_eq!(
            a.lend_begin_block().tempo,
            Bpm::of(143.5),
            "set_tempo is a map"
        );
    }

    /// A tempo stored after the block began does not land at a command's cut: the
    /// position a timed `Play` reports carries the tempo `begin_block` read, and
    /// only the command's own effect (rolling). The store lands at the next block.
    #[test]
    fn a_tempo_stored_after_begin_block_waits_for_the_next_block() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut a = t.audio(SampleRate::SR_48K).expect("first");
        t.motion
            .schedule(At::Frame(Frame::new(100)), MotionCommand::Play)
            .expect("room");
        let start = a.lend_begin_block();
        assert_eq!(start.tempo, Bpm::of(120.0));
        a.drain_commands();
        t.settings.set_tempo(Bpm::of(200.0));
        let (_, at_cut) = a.lend_apply_command(0, false);
        assert!(at_cut.playing, "the command's effect");
        assert_eq!(at_cut.tempo, Bpm::of(120.0), "not the stored tempo");
        assert_eq!(a.lend_begin_block().tempo, Bpm::of(200.0), "the next block");
    }

    fn take(n: u64) -> TakeId {
        TakeId::of(n)
    }

    /// Sends `recording` at `at`.
    fn send(t: &Transport, at: At, recording: Recording) {
        t.motion.schedule(at, recording).expect("room");
    }

    /// Applies the pending `recording` command at its frame, and reports the
    /// transport from there.
    fn fire(a: &mut TransportAudio, recording: Recording) -> TransportPosition {
        a.drain_commands();
        let i = a
            .pending()
            .iter()
            .position(|c| c.command() == TransportCommand::Recording(recording))
            .expect("the command is pending");
        a.lend_apply_command(i, false).1
    }

    /// **A take's start is the transport recording from its frame, and the
    /// transport records while any take does.** Takes 1 and 2 overlap: the
    /// block reads not recording before the first start's frame and
    /// recording from it, through the first stop, until the second's. A stop
    /// of a take that never started changes nothing, before or during.
    #[test]
    fn the_transport_records_from_a_starts_frame_until_the_last_stop() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut a = t.audio(SampleRate::SR_48K).expect("first");
        for (frame, recording) in [
            (100, Recording::Start(take(1))),
            (200, Recording::Start(take(2))),
            (300, Recording::Stop(take(1))),
            (400, Recording::Stop(take(2))),
            (50, Recording::Stop(take(9))),
        ] {
            send(&t, At::Frame(Frame::new(frame)), recording);
        }
        assert!(!a.lend_begin_block().recording(), "before the first start");
        assert!(!fire(&mut a, Recording::Stop(take(9))).recording());
        assert!(fire(&mut a, Recording::Start(take(1))).recording());
        assert!(fire(&mut a, Recording::Start(take(2))).recording());
        assert!(
            fire(&mut a, Recording::Stop(take(1))).recording(),
            "take 2 still records"
        );
        assert!(!fire(&mut a, Recording::Stop(take(2))).recording());
        assert_eq!(t.motion.scheduled_outstanding(), 0);
    }

    /// An engine rebuilt over the same transport keeps the takes recording.
    #[test]
    fn the_takes_recording_outlive_the_audio_side() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut first = t.audio(SampleRate::SR_48K).expect("first");
        send(&t, At::NextBlock, Recording::Start(take(1)));
        assert!(fire(&mut first, Recording::Start(take(1))).recording());
        drop(first);
        let mut next = t.audio(SampleRate::SR_48K).expect("handed back on drop");
        assert!(next.lend_begin_block().recording());
        send(&t, At::NextBlock, Recording::Stop(take(1)));
        assert!(!fire(&mut next, Recording::Stop(take(1))).recording());
    }

    /// A stop drops the start of its take that still waits for a far frame:
    /// the credit comes back, nothing counts late, and the take never records.
    #[test]
    fn a_stop_drops_its_takes_waiting_start() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut a = t.audio(SampleRate::SR_48K).expect("first");
        send(
            &t,
            At::Frame(Frame::new(1_000_000)),
            Recording::Start(take(1)),
        );
        send(
            &t,
            At::Frame(Frame::new(1_000_000)),
            Recording::Start(take(2)),
        );
        send(&t, At::NextBlock, Recording::Stop(take(1)));
        assert!(!fire(&mut a, Recording::Stop(take(1))).recording());
        assert_eq!(a.pending().len(), 1, "take 2's start waits on");
        assert_eq!(a.pending()[0].command(), Recording::Start(take(2)).into());
        assert_eq!(t.motion.scheduled_outstanding(), 1, "the credit came back");
        assert_eq!(t.motion.late_command_count(), 0);
    }

    /// A cancel takes back what waits for a time but spares a take's start,
    /// as it spares a next-block command.
    #[test]
    fn a_cancel_spares_a_takes_start() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut a = t.audio(SampleRate::SR_48K).expect("first");
        send(&t, At::Frame(Frame::new(500)), Recording::Start(take(1)));
        t.motion
            .schedule(At::Frame(Frame::new(500)), MotionCommand::Play)
            .expect("room");
        t.motion.cancel_scheduled();
        a.drain_commands();
        assert_eq!(a.pending().len(), 1, "the play was taken back");
        assert!(fire(&mut a, Recording::Start(take(1))).recording());
    }

    /// A rolling transport over `points` (steps), at `rate`.
    fn rolling(rate: f64, points: &[(f64, f64)]) -> (Transport, TransportAudio) {
        rolling_on(rate, steps(points))
    }

    /// A rolling transport over `map`, at `rate`.
    fn rolling_on(rate: f64, map: std::sync::Arc<TempoMap>) -> (Transport, TransportAudio) {
        let t = Transport::new(SampleRate::of(rate));
        t.settings.set_tempo_map(map);
        let mut a = t.audio(SampleRate::of(rate)).expect("first");
        t.motion
            .schedule(At::NextBlock, MotionCommand::Play)
            .expect("room");
        a.drain_commands();
        let _ = a.lend_begin_block();
        let _ = a.lend_apply_command(0, false);
        (t, a)
    }

    /// Roll `frames` as one block, and report the transport at its start.
    fn roll(a: &mut TransportAudio, frames: usize) -> tutti_graph::TransportPosition {
        let t = a.lend_begin_block();
        a.lend_advance(frames, &t);
        t
    }

    /// The look ahead finds the step on its frame and with its transport: 100
    /// frames before beat 4 of 120 to 60 BPM at 48 kHz, a 512-frame block has
    /// one change at offset 100, on beat 4 exactly at 60 BPM. With no room the
    /// block ends there; a constant map has none.
    #[test]
    fn the_look_ahead_finds_a_step_and_cuts_a_block_that_cannot_hold_it() {
        let (_t, mut a) = rolling(48_000.0, &[(0.0, 120.0), (4.0, 60.0)]);
        let _ = roll(&mut a, 96_000 - 100);
        let t = a.lend_begin_block();
        let ahead = a.lend_tempo_ahead(512, &t, 8);
        assert_eq!(ahead.limit, None);
        let [change] = ahead.changes.as_slice() else {
            panic!("one change, got {:?}", ahead.changes);
        };
        assert_eq!(change.at.index(), 100);
        assert_eq!(change.to.tempo, Bpm::of(60.0));
        assert_eq!(change.to.beat(), Beat::new(4.0));
        assert_eq!(change.to.tempo, Bpm::of(60.0));
        let full = a.lend_tempo_ahead(512, &t, 0);
        assert!(full.changes.is_empty());
        assert_eq!(
            full.limit,
            Some(100),
            "the block ends where it would be 1 too many"
        );
        // Moving nothing: the same look again.
        assert_eq!(a.lend_tempo_ahead(512, &t, 8).changes, ahead.changes);

        let t = Transport::new(SampleRate::SR_48K);
        let mut c = t.audio(SampleRate::SR_48K).expect("first");
        let blk = c.lend_begin_block();
        assert!(c.lend_tempo_ahead(512, &blk, 8).changes.is_empty());
    }

    /// A map published while rolling is taken at the next block start without
    /// moving the playhead: the beat of that block is the beat the last one
    /// ended on (bit for bit), at the tempo the map has there, and the step
    /// that follows lands on its frame.
    #[test]
    fn a_swapped_in_step_map_re_seats_at_the_current_beat() {
        let t = Transport::new(SampleRate::SR_48K);
        let mut a = t.audio(SampleRate::SR_48K).expect("first");
        t.motion
            .schedule(At::NextBlock, MotionCommand::Play)
            .expect("room");
        a.drain_commands();
        let _ = a.lend_begin_block();
        let _ = a.lend_apply_command(0, false);
        let _ = roll(&mut a, 24_000); // beat 1 at 120
        let ended = {
            let blk = a.lend_begin_block();
            blk.beat()
        };
        assert_eq!(ended, Beat::new(1.0));
        t.settings.set_tempo_map(steps(&[(0.0, 90.0), (2.0, 60.0)]));
        let blk = a.lend_begin_block();
        assert_eq!(blk.beat(), Beat::new(1.0), "no jump");
        assert_eq!(blk.tempo, Bpm::of(90.0), "the map's tempo here");
        // Beat 2 is 32 000 frames on at 90 BPM.
        let _ = roll(&mut a, 31_999);
        assert_eq!(a.lend_begin_block().tempo, Bpm::of(90.0));
        let _ = roll(&mut a, 1);
        let blk = a.lend_begin_block();
        assert_eq!(blk.tempo, Bpm::of(60.0));
        assert_eq!(blk.beat(), Beat::new(2.0));
    }

    fn steps(points: &[(f64, f64)]) -> std::sync::Arc<TempoMap> {
        use tutti_tempo::{TempoPoint, Transition};
        std::sync::Arc::new(
            TempoMap::new(points.iter().map(|&(beat, bpm)| TempoPoint {
                beat: Beat::new(beat),
                tempo: Bpm::of(bpm),
                transition: Transition::Step,
            }))
            .expect("a map"),
        )
    }

    /// A rate change re-seats at the current beat on the map's timeline at the
    /// new rate: beat 6 of a 60-to-240 step at beat 8 is 2 beats of 60 BPM, or
    /// 192 000 frames, from the step at 96 kHz (and the clock had counted 144 000
    /// at 48 kHz to get here).
    #[test]
    fn a_rate_change_re_seats_on_the_maps_timeline() {
        let (_t, mut a) = rolling(48_000.0, &[(0.0, 120.0), (4.0, 60.0), (8.0, 240.0)]);
        // 4 beats at 120, then 2 at 60: beat 6.
        let _ = roll(&mut a, 96_000 + 96_000);
        assert_eq!(a.lend_begin_block().beat(), Beat::new(6.0));
        a.follow_rate(SampleRate::SR_96K);
        let blk = a.lend_begin_block();
        assert_eq!((blk.beat(), blk.tempo), (Beat::new(6.0), Bpm::of(60.0)));
        let _ = roll(&mut a, 191_999);
        assert_eq!(a.lend_begin_block().tempo, Bpm::of(60.0));
        let _ = roll(&mut a, 1);
        let blk = a.lend_begin_block();
        assert_eq!((blk.beat(), blk.tempo), (Beat::new(8.0), Bpm::of(240.0)));
    }

    /// A rate change inside a ramp re-seats at the current beat on the map's
    /// timeline at the new rate, and takes the chord of the cell it lands in
    /// there. The ramp is
    /// 120 to 60 BPM over 4 beats (`r0` 2, `r1` 1 beats/s, `T = 8 / 3` s,
    /// `a = -3 / 8`, `B(s) = 2 s - 3 s^2 / 16`); after 60 000 frames at 48 kHz
    /// the playhead keeps its beat, the map frame at 96 kHz is the first at or
    /// after `s(beat) x 96 000` (`s = (sqrt(4 - 3 b / 4) - 2) / a`), the block's
    /// tempo is the chord of `B` over that frame's 128-frame cell, and at the
    /// cell's edge the playhead is on `B` exactly, at the next chord.
    #[test]
    fn a_rate_change_inside_a_ramp_takes_the_chord_at_the_new_rate() {
        use tutti_tempo::{TempoPoint, Transition};
        let map = std::sync::Arc::new(
            TempoMap::new([
                TempoPoint {
                    beat: Beat::new(0.0),
                    tempo: Bpm::of(120.0),
                    transition: Transition::RampTime { end: None },
                },
                TempoPoint {
                    beat: Beat::new(4.0),
                    tempo: Bpm::of(60.0),
                    transition: Transition::Step,
                },
            ])
            .expect("a map"),
        );
        let b_of = |s: f64| 2.0 * s - 3.0 * s * s / 16.0;
        let (_t, mut a) = rolling_on(48_000.0, map);
        let _ = roll(&mut a, 60_000);
        let beat = a.lend_begin_block().beat();
        assert!(beat.get() > 1.0 && beat.get() < 3.0, "mid-ramp: {beat:?}");

        a.follow_rate(SampleRate::SR_96K);
        let blk = a.lend_begin_block();
        assert_eq!(blk.beat(), beat, "no jump");
        let s = ((4.0 - 0.75 * beat.get()).sqrt() - 2.0) / (-3.0 / 8.0);
        let m = tutti_music::first_frame_at_or_after(s * 96_000.0) as u64;
        let k = m / 128;
        let at = |frame: u64| b_of(frame as f64 / 96_000.0);
        let chord = |k: u64| (at((k + 1) * 128) - at(k * 128)) * 60.0 * 96_000.0 / 128.0;
        assert!((blk.tempo.get() - chord(k)).abs() < 1e-6, "{:?}", blk.tempo);
        // The 48 kHz cell of the same beat has another chord.
        assert!((blk.tempo.get() - chord(k / 2 + k % 2)).abs() > 1e-3);

        // To the cell's edge: on the map's beat, at the next chord.
        let _ = roll(&mut a, ((k + 1) * 128 - m) as usize);
        let blk = a.lend_begin_block();
        assert!((blk.beat().get() - at((k + 1) * 128)).abs() < 1e-10);
        assert!((blk.tempo.get() - chord(k + 1)).abs() < 1e-6);
    }

    /// A map has no hysteresis: a step from 120 to 120.0005 BPM (inside the
    /// clock's 0.001 band for a scalar tempo) is taken on its frame.
    #[test]
    fn a_map_is_followed_without_the_scalar_hysteresis() {
        let (_t, mut a) = rolling(48_000.0, &[(0.0, 120.0), (1.0, 120.0005)]);
        let _ = roll(&mut a, 24_000);
        assert_eq!(a.lend_begin_block().tempo, Bpm::of(120.0005));
    }

    /// A map with steps published before the audio side is taken (an engine
    /// built after the project set its tempo, or rebuilt by a device restart)
    /// is followed from its first block: the step lands on its frame.
    #[test]
    fn a_map_published_before_the_audio_side_is_taken_is_followed() {
        let (t, a) = rolling(48_000.0, &[(0.0, 120.0), (4.0, 60.0)]);
        drop(a);
        let mut a = t.audio(SampleRate::SR_48K).expect("handed back on drop");
        let mut seen = Vec::new();
        for _ in 0..3 {
            let blk = a.lend_begin_block();
            seen.push((blk.tempo, blk.beat()));
            a.lend_advance(96_000, &blk);
        }
        // The seat survives the restart on the beat the clock stood on (0:
        // the drop kept the machine rolling at the start).
        assert_eq!(seen[0].0, Bpm::of(120.0));
        assert_eq!(
            seen[1],
            (Bpm::of(60.0), Beat::new(4.0)),
            "the step, on its frame"
        );
        assert_eq!(
            seen[2].1,
            Beat::new(6.0),
            "96 000 frames at 60 BPM are 2 beats"
        );
    }
}
