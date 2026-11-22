//! Transport-driven MIDI **sync master**: generate outbound MIDI Beat Clock
//! (24 PPQN) and MIDI Time Code so external gear follows *this* transport.
//!
//! `ClockMaster` is the symmetric inverse of the sync *decoders*
//! ([`MidiClockDecoder`](crate::MidiClockDecoder) /
//! [`MtcDecoder`](crate::MtcDecoder)): where they read an
//! incoming stream to derive transport state, this reads the transport to
//! *produce* the stream. It is ticked once per transport segment of a block,
//! on the audio thread, by a [`SyncOut`](crate::SyncOut), and stamps each
//! event it emits with a sample-accurate [`Offset`], so receiving gear
//! locks tightly instead of chasing frame-quantised jitter.
//!
//! **The tempo is the published map's**: the engine hands
//! the node a segment for every cell where the chord tempo changes, seated on
//! the map's exact beat, and `tick` reads `transport.tempo` of each. A
//! segment that starts where the previous ended (within `SEEK_EPSILON`)
//! is a continuation, so a ramp's cell edge or a step is no locate; the
//! ticks are the first frames at or after beat `k / 24` on the playhead, which
//! is on the map's frame at every cell start and within a frame of it inside
//! one (`tests/clock_tempo_map.rs`).
//!
//! Emitted (all MIDI 2.0 UMP System messages, M2-104 §7.6):
//! - **Timing Clock** (0xF8) at every 1/24-beat while playing.
//! - **Start** (0xFA) / **Continue** (0xFB) / **Stop** (0xFC) on transport
//!   edges — Start when playback begins at beat 0, Continue otherwise.
//! - **Song Position** (0xF2) on Continue, on seek-while-playing and on the
//!   frame a loop wraps, in MIDI beats (1/16 notes).
//!
//! A loop wrap is a locate on its own frame: a Song Position there (also on a
//! block's first frame, when the previous block ended on the wrap), the next
//! ticks from the wrapped beat, and the MTC grid restarts. A Song Position
//! counts sixteenths and rounds to the nearest, so a loop start off a
//! sixteenth leaves a receiver up to 1/8 beat behind the sender's tick grid,
//! as after any locate. The restarted grid puts its first piece 0 on the next
//! even frame of the timeline from the wrapped beat, so a loop start that is
//! not on an even frame leaves up to two frames with no quarter-frames.
//! - **Set Tempo** and **Set Time Signature** (Flex Data, M2-104 §7.5.3 and
//!   §7.5.4, when [`SyncMessages::flex`] is set) and **Set Metronome** (§7.5.5,
//!   when [`SyncMessages::metronome`] is set), on the clock's Group, with or
//!   without the clock bytes enabled; a Set Metronome goes on the frames a Set
//!   Time Signature would. Set Tempo goes out on the first 24-PPQN
//!   tick position at or after the tempo in force changes, Set Time Signature on
//!   the first bar boundary at or after the meter in force changes. Where
//!   playback starts the tempo goes out on the first tick position (the start
//!   beat's own, when it is on one) and the meter on the first bar boundary: a
//!   start from the top is both, a Continue mid-bar states the meter at the next
//!   bar boundary, never off a boundary.
//! - **MTC quarter-frames** (0xF1) at frame-rate×4, cycling the 8 SMPTE nibbles
//!   (when [`SyncMessages::mtc`] is set).
//! - **MTC Full Messages** (Real Time `01 01`, when `mtc` is set) on every
//!   locate: a cue while stopped, the start from a position no cue stated, a
//!   jump while playing and a loop wrap. The quarter-frame cycle restarts at
//!   piece 0 after it.

use tutti_graph::{Offset, TransportPosition, UmpAt};
use tutti_music::{
    ClockPass, ClockPassEnd, Meter, MeterMap, TimeSignature, first_frame_at_or_after,
};
use tutti_ump::{Flex, Message, MidiGroup, System, Ump};
use tutti_units::{
    BarCount, Beat, BeatDuration, Bpm, Losses, SampleRate, SampleSpan, Samples, Seconds,
};

use tutti_midi_protocol::{
    MtcFullMessage, MtcRate, MtcTimeCode, QuarterFrameCycle, QuarterFramePiece, SmpteTimecode,
    flex_metronome, flex_tempo_field, flex_time_signature_wire, song_position,
};

use super::sync_messages::SyncMessages;

/// Below this beat, a play edge is treated as "from the top" → Start (0xFA)
/// rather than Continue (0xFB). Wide enough to absorb float jitter at beat 0.
const START_EPSILON: BeatDuration = BeatDuration::new(1e-6);
/// A start beat further than this from where the previous block ended (while
/// playing) is a seek, not the normal run-on of the next block: it triggers a
/// fresh Song Position.
const SEEK_EPSILON: BeatDuration = BeatDuration::new(1e-3);

/// Generates outbound MIDI clock / timecode from the transport: the Beat
/// Clock and MTC half of a [`SyncOut`](crate::SyncOut), which owns it.
///
/// Everything here is plain state only [`tick`](Self::tick) uses, so it takes
/// `&mut self`: no atomics, no locks, no allocation. What is sent comes with
/// each tick as a [`SyncMessages`]; a frame-rate change it finds there rebases
/// the MTC grid on the audio thread.
#[derive(Debug)]
pub(crate) struct ClockMaster {
    /// UMP group stamped on every emitted event.
    group: MidiGroup,
    /// The device rate: what turns the transport's beats into frame offsets.
    sample_rate: SampleRate,
    /// The MTC frame rate in force, as of the last tick.
    fps: MtcRate,
    /// What the previous tick saw of the transport.
    prev: PrevTick,
    /// The MTC quarter-frame grid.
    mtc: MtcGrid,
    /// What the Flex messages last stated.
    flex: FlexOut,
    /// The Song Position Pointers and MTC timecodes not sent since the host
    /// last took the count ([`take_losses`](Self::take_losses)): a position
    /// the 14-bit pointer or a timecode cannot name. MTC with no time (before
    /// beat 0) counts once from each grid restart, not per quarter-frame.
    refused: Losses,
}

/// What the Flex output has stated to the receiver, and what it has seen of
/// the transport.
#[derive(Debug)]
struct FlexOut {
    /// `playing` at the previous tick, whatever the settings: a start states
    /// tempo and meter afresh.
    was_playing: bool,
    /// The Set Tempo field last sent; `None` before the first, after a start
    /// and while Flex is off.
    tempo: Option<u32>,
    /// The meter last sent, under the same reset.
    meter: Option<TimeSignature>,
    /// The meter whose Set Metronome was last sent, under the same reset.
    metronome: Option<TimeSignature>,
}

impl FlexOut {
    const SILENT: Self = Self {
        was_playing: false,
        tempo: None,
        meter: None,
        metronome: None,
    };
}

/// The transport as the previous tick saw it, for edge and locate detection.
#[derive(Debug)]
struct PrevTick {
    /// `playing` at the previous tick, for stopped to playing edge detection
    /// (the transport exposes no edges of its own).
    playing: bool,
    /// The beat the previous tick's block ended on, a loop wrap in it
    /// included: what the next block's start is compared with to tell a
    /// locate from ordinary playback. The previous block's end, not this
    /// block's figure: the block size, the tempo or (on a device restart) the
    /// sample rate may have moved in between, and a wrap moves the beat
    /// backwards.
    end: Beat,
    /// The previous tick's block ended on a loop wrap, on its last frame's
    /// successor: that frame is the next block's first, whose clock already
    /// stands on the wrapped beat, so the wrap shows in no pass of its own
    /// and `end` equals the start. The next tick sends its Song Position.
    wrapped: bool,
    /// The beat a Full Message last stated while the transport stood still
    /// (or where it stopped): what a stopped transport's position is compared
    /// with to tell a cue from standing still. `None` while rolling, and
    /// before the first look, so the first stopped tick states its position.
    cued: Option<Beat>,
}

impl PrevTick {
    const FRESH: Self = Self {
        playing: false,
        end: Beat::new(0.0),
        wrapped: false,
        cued: None,
    };
}

/// The MTC quarter-frame grid.
#[derive(Debug)]
struct MtcGrid {
    /// The MTC quarter-frame grid, in closed form rather than accumulated
    /// (the frame, not the beat, is the source of truth): quarter-frame `k`
    /// (counted from the last transport edge or locate, so `k & 7` is its
    /// nibble) is due `lead + (k - base) × rate / (4 × fps)` frames
    /// into the current grid segment, whose frames `frames` counts. A
    /// rate or fps change starts a new segment at the next quarter-frame due,
    /// with its lead rescaled to the same wall-clock time; an edge or locate
    /// restarts at quarter-frame 0 on the block's first frame.
    base: u64,
    /// The next quarter-frame to emit (see `base`).
    next: u64,
    /// Frames rolled in the current grid segment.
    frames: u64,
    /// Where quarter-frame `base` is due in the segment, in frames.
    lead: f64,
    /// The eight Quarter Frames of the timecode being sent, in piece order. Set
    /// once at piece 0, so one cycle carries one position.
    cycle: QuarterFrameCycle,
    /// The grid was just restarted: its first piece 0 is still to be placed
    /// on an even frame ([`ClockMaster::tick_mtc`]).
    align: bool,
    /// Nothing of the current cycle is sent, and its refusal is counted
    /// already: its piece 0 had no timecode to state, or every pass since the
    /// grid restarted has no MIDI time (before beat 0). No receiver reads half
    /// of a position or the previous cycle's.
    skipped: bool,
}

impl MtcGrid {
    const RESTARTED: Self = Self {
        base: 0,
        next: 0,
        frames: 0,
        lead: 0.0,
        cycle: QuarterFrameCycle::ZERO,
        align: true,
        skipped: false,
    };
}

impl ClockMaster {
    /// A System message on this master's group.
    fn system(&self, message: System) -> Ump {
        Message::System(self.group, message).into()
    }

    /// A clock sending on `group` (every event it emits is addressed to it: a
    /// clock is the Group's, M2-104 section 7.6) and placing events at
    /// `sample_rate`.
    pub(crate) fn new(group: MidiGroup, sample_rate: SampleRate) -> Self {
        Self {
            group,
            sample_rate,
            fps: SyncMessages::OFF.rate,
            prev: PrevTick::FRESH,
            mtc: MtcGrid::RESTARTED,
            flex: FlexOut::SILENT,
            refused: Losses::NONE,
        }
    }

    /// The messages [refused](Losses::refused) since the last call, then
    /// none: a Song Position Pointer for a beat past its 14-bit field (4096 beats) or before the
    /// start, and an MTC position for a time before zero or NaN (once from each
    /// grid restart while the quarter-frames have none). Each is skipped, never
    /// sent clamped to a position the transport is not at.
    pub(crate) fn take_losses(&mut self) -> Losses {
        std::mem::take(&mut self.refused)
    }

    /// A Song Position Pointer for `beat` on `at`, or a count of one refusal
    /// when `beat` is outside the pointer's field.
    fn emit_song_position(&mut self, beat: Beat, at: Offset, emit: &mut impl FnMut(UmpAt)) {
        match song_position(beat) {
            Some(position) => emit((at, self.system(System::SongPosition(position)))),
            None => self.refused += Losses::refused(1),
        }
    }

    /// Returns the clock to what it was before its first tick: the transport
    /// it last saw, the MTC grid and what the Flex messages stated are
    /// forgotten, so the next tick states its position afresh. The Group, the
    /// rate and the frame rate are kept.
    pub(crate) fn reset(&mut self) {
        self.prev = PrevTick::FRESH;
        self.mtc = MtcGrid::RESTARTED;
        self.flex = FlexOut::SILENT;
    }

    /// Moves the clock to a device running at `sample_rate`: what a device
    /// restart at a new rate does, with no callback running.
    ///
    /// The 24-PPQN ticks are placed from the transport's beat, so from the
    /// next block they land on the new rate's frames: at 120 BPM a tick
    /// every 1 000 frames at 48 kHz, not the 918.75 of 44.1 kHz, which would
    /// run the receiving gear about 8.8% fast. The MTC quarter-frame grid is
    /// counted in frames, so it starts a new segment at the next
    /// quarter-frame due, rescaled to the same wall-clock time: that
    /// quarter-frame stays where it was due.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        let old = std::mem::replace(&mut self.sample_rate, sample_rate);
        self.rebase_mtc(old, self.fps, sample_rate);
    }

    /// Frames from the MTC grid segment's first frame to quarter-frame `k`,
    /// at `rate` and `fps`: closed form, never accumulated.
    fn mtc_due(&self, k: u64, rate: SampleRate, fps: MtcRate) -> f64 {
        self.mtc.lead + k.saturating_sub(self.mtc.base) as f64 * frame_span(fps, rate).get() / 4.0
    }

    /// Start a new MTC grid segment at the next quarter-frame due, the
    /// frames left until it rescaled from `old_rate`/`old_fps` to the same
    /// wall-clock time at `new_rate` (a quarter-frame already due stays
    /// due). `old_fps` only places the pending one: a new fps spaces the
    /// ones after it.
    fn rebase_mtc(&mut self, old_rate: SampleRate, old_fps: MtcRate, new_rate: SampleRate) {
        let next = self.mtc.next;
        let left = self.mtc_due(next, old_rate, old_fps) - self.mtc.frames as f64;
        self.mtc.base = next;
        self.mtc.lead = left * new_rate.get() / old_rate.get();
        self.mtc.frames = 0;
    }

    /// Restart the MTC grid at quarter-frame 0 (piece 0, what a decoder
    /// needs to lock) on the next block's first frame: a transport edge or
    /// a locate.
    fn restart_mtc(&mut self) {
        let cycle = self.mtc.cycle;
        self.mtc = MtcGrid {
            cycle,
            ..MtcGrid::RESTARTED
        };
    }

    /// Generate the clock/timecode of `block` frames starting with the
    /// transport at `transport`, handing each event (its [`Offset`] from
    /// the first of those frames) to `emit`. Call once per stretch of
    /// unchanging transport: a block, or each of its segments.
    ///
    /// Events come out in the order generated, not by offset: in each pass
    /// (the block cut at a loop wrap) the MTC quarter-frames follow its ticks.
    pub(crate) fn tick(
        &mut self,
        messages: SyncMessages,
        meter: Option<&MeterMap>,
        transport: &TransportPosition,
        block: Samples,
        emit: &mut impl FnMut(UmpAt),
    ) {
        // The passes below index and split by frame count.
        let block_size = block.get();
        // Once per block: a rate moved mid-block would split it in two. A
        // counted transport carries the rate its block runs at, and that is
        // the one its clock is rebuilt at (a master re-rated between the
        // host's block and this tick would otherwise mix the two).
        let sample_rate = transport
            .origin()
            .map_or(self.sample_rate, |o| o.sample_rate);
        // A frame rate moved is a new MTC grid segment, started here on the
        // audio thread.
        if messages.rate != self.fps {
            self.rebase_mtc(self.sample_rate, self.fps, self.sample_rate);
            self.fps = messages.rate;
        }
        // A start states tempo and meter afresh; Flex off forgets them, so a
        // later enable states them too.
        let flex_started = transport.playing && !self.flex.was_playing;
        self.flex.was_playing = transport.playing;
        if flex_started || !messages.flex {
            self.flex.tempo = None;
            self.flex.meter = None;
        }
        if flex_started || !messages.metronome {
            self.flex.metronome = None;
        }
        if !messages.clock && !messages.mtc && messages.any_flex() && block_size != 0 {
            // Flex alone: no clock bytes or timecode, and the clock's own edge
            // and locate state is left as it was.
            if transport.playing {
                let beats_per_sample = tutti_music::beats_per_sample(transport.tempo, sample_rate);
                let region = transport.looping;
                let mut clock = transport.clock(sample_rate);
                for pass in clock.passes(Samples::new(block_size), region) {
                    let Some(meter) = meter else { break };
                    self.send_flex(
                        messages,
                        meter,
                        &pass,
                        block_size,
                        beats_per_sample,
                        transport.tempo,
                        emit,
                    );
                }
            }
            self.prev.playing = transport.playing;
            self.prev.wrapped = false;
            return;
        }
        if !(messages.clock || messages.mtc) || block_size == 0 {
            // Keep prev.playing honest so re-enabling mid-playback emits a fresh
            // Start/Continue rather than silently assuming playback was already
            // under way.
            self.prev.playing = transport.playing;
            self.prev.wrapped = false;
            return;
        }

        let playing = transport.playing;
        let was_playing = std::mem::replace(&mut self.prev.playing, playing);
        let beat = transport.beat();

        // --- transport edges -------------------------------------------------
        if playing && !was_playing {
            if messages.clock {
                if (beat - Beat::new(0.0)).abs() <= START_EPSILON {
                    emit((Offset::ZERO, self.system(System::Start)));
                } else {
                    self.emit_song_position(beat, Offset::ZERO, emit);
                    emit((Offset::ZERO, self.system(System::Continue)));
                }
            }
            // The receiver is told where it starts from, unless a cue already
            // did; then the grid realigns to the (possibly non-zero) start beat.
            if messages.mtc && !self.is_cued_at(beat) {
                self.send_full_message(beat, transport, sample_rate, Offset::ZERO, emit);
            }
            self.prev.cued = None;
            self.restart_mtc();
        } else if !playing && was_playing {
            if messages.clock {
                emit((Offset::ZERO, self.system(System::Stop)));
            }
            self.prev.wrapped = false;
            self.prev.cued = Some(beat);
            return;
        }

        if !playing {
            self.prev.wrapped = false;
            // A locate while stopped is a cue: the receiver is told where the
            // transport stands by a Full Message, as no Quarter Frame runs.
            if messages.mtc && !self.is_cued_at(beat) {
                self.send_full_message(beat, transport, sample_rate, Offset::ZERO, emit);
                self.prev.cued = Some(beat);
            }
            return;
        }

        let tempo = transport.tempo;
        // Kept as the `BeatDuration` the shared derivation returns rather than
        // unwrapped to f64: this is a *span* of beats per sample, while `beat`
        // is a *position*. Both are beats-denominated and neither is an
        // `Hz` — a beat-synced rate is beats-per-cycle, the inverse of a
        // frequency. Unwrapping either lets the two mix silently.
        let beats_per_sample = tutti_music::beats_per_sample(tempo, sample_rate);

        // --- seek while playing ---------------------------------------------
        // A block starts where the previous one ended; any other start, either
        // direction, is a locate. So is a block that starts on a wrap: the
        // previous block ended on it, and one Song Position serves it and a
        // locate on the same frame.
        let prev_end = self.prev.end;
        let is_edge = playing && !was_playing;
        let located = (beat - prev_end).abs() > SEEK_EPSILON;
        let wrapped = self.prev.wrapped;
        if !is_edge && (located || wrapped) {
            if messages.clock {
                self.emit_song_position(beat, Offset::ZERO, emit);
            }
            if messages.mtc {
                self.send_full_message(beat, transport, sample_rate, Offset::ZERO, emit);
            }
            self.restart_mtc();
        }

        // --- per pass: 24-PPQN clock ticks, MTC ------------------------------
        // The block is cut at each loop wrap (`FrameClock::passes`) and each
        // pass is placed from its own first beat, so the ticks after a wrap
        // follow the wrapped playhead instead of running on past the loop's
        // end. A wrap is a locate on its own frame: a Song Position there, and
        // the MTC grid restarts on it.
        let region = transport.looping;
        let mut clock = transport.clock(sample_rate);
        let mut ends_on_wrap = false;
        for pass in clock.passes(Samples::new(block_size), region) {
            ends_on_wrap = pass.end == ClockPassEnd::Wrap;
            if !pass.start.is_zero() {
                if messages.clock {
                    self.emit_song_position(
                        pass.beat,
                        frame_offset(pass.start.get(), block_size),
                        emit,
                    );
                }
                if messages.mtc {
                    self.send_full_message(
                        pass.beat,
                        transport,
                        sample_rate,
                        frame_offset(pass.start.get(), block_size),
                        emit,
                    );
                }
                self.restart_mtc();
            }
            if messages.clock {
                self.tick_clock(&pass, block_size, beats_per_sample, emit);
            }
            if let Some(meter) = meter.filter(|_| messages.any_flex()) {
                self.send_flex(
                    messages,
                    meter,
                    &pass,
                    block_size,
                    beats_per_sample,
                    tempo,
                    emit,
                );
            }
            if messages.mtc {
                self.tick_mtc(&pass, block_size, sample_rate, beats_per_sample, emit);
            }
        }
        self.prev.end = clock.beat();
        self.prev.wrapped = ends_on_wrap;
    }

    /// Whether a Full Message already stated `beat` (within a seek's width).
    fn is_cued_at(&self, beat: Beat) -> bool {
        self.prev
            .cued
            .is_some_and(|cued| (beat - cued).abs() <= SEEK_EPSILON)
    }

    /// Emit the MTC Full Message (Real Time `01 01`) for the frame nearest
    /// `beat` at the clock's MTC rate, on `at`: the form a generator uses to
    /// locate or cue, where Quarter Frames only describe a position that runs.
    /// The position is `beat` at the transport's tempo from beat 0, the grid's
    /// own timeline.
    fn send_full_message(
        &mut self,
        beat: Beat,
        transport: &TransportPosition,
        sample_rate: SampleRate,
        at: Offset,
        emit: &mut impl FnMut(UmpAt),
    ) {
        let per_sample = tutti_music::beats_per_sample(transport.tempo, sample_rate);
        let Some(position) = samples_at(beat, per_sample).and_then(|samples| {
            MtcTimeCode::at(Seconds::new(samples / sample_rate.get()), self.fps)
        }) else {
            self.refused += Losses::refused(1);
            return;
        };
        for packet in MtcFullMessage::from(position).packets(self.group) {
            emit((at, packet));
        }
    }

    /// Emit a 0xF8 at every 1/24-beat boundary playback reaches inside `pass`,
    /// on its frame.
    ///
    /// Placed by the engine's one beat→frame rule
    /// (`first_frame_at_or_after`) and compared as integer
    /// frames, not as beats against the pass's end: a tick on a block
    /// boundary is reached on the next block's first frame (offset `len`
    /// here, 0 there), so it goes out once. Comparing beats, the previous
    /// block's end and this block's start round independently, and a
    /// boundary tick was sent twice or not at all.
    ///
    /// Where playback starts, continues, locates or wraps on a tick boundary,
    /// that tick goes out on the first frame, after the Start / Continue /
    /// Song Position sent for it (same offset, later in the queue). MIDI 1.0
    /// (MMA, System Real Time Messages, Start and Continue): a receiver
    /// begins on the first Timing Clock after Start / Continue, so the
    /// clock of the start beat must be sent; skipping it leaves every
    /// receiving device one tick behind. Off a boundary, the first tick is
    /// the next boundary's.
    fn tick_clock(
        &self,
        pass: &ClockPass,
        block_size: usize,
        beats_per_sample: BeatDuration,
        emit: &mut impl FnMut(UmpAt),
    ) {
        for k in tick_frames(pass, beats_per_sample) {
            emit((
                frame_offset(pass.start.get() + k, block_size),
                self.system(System::TimingClock),
            ));
        }
    }

    /// Emit the Flex tempo and meter due in `pass`: a Set Tempo on the first
    /// 24-PPQN tick frame (the grid [`tick_clock`](Self::tick_clock) places) if
    /// `tempo` is not what was last stated, and a Set Time Signature on each
    /// bar boundary whose meter is not, followed on the same frame by a Set
    /// Metronome when [`SyncMessages::metronome`] is on and that meter is not
    /// the one it last stated. The tempo is the segment's, constant
    /// over the pass, so a change waits for the first tick at or after the
    /// segment's start, in this pass or a later one. `meter` is the project
    /// meter, lent once by [`tick`](Self::tick) for every pass of the block.
    fn send_flex(
        &mut self,
        messages: SyncMessages,
        meter: &MeterMap,
        pass: &ClockPass,
        block_size: usize,
        beats_per_sample: BeatDuration,
        tempo: Bpm,
        emit: &mut impl FnMut(UmpAt),
    ) {
        let field = flex_tempo_field(tempo);
        if messages.flex
            && self.flex.tempo != Some(field)
            && let Some(k) = tick_frames(pass, beats_per_sample).next()
        {
            emit((
                frame_offset(pass.start.get() + k, block_size),
                Message::Flex(self.group, Flex::SetTempo(field)).into(),
            ));
            self.flex.tempo = Some(field);
        }
        // The frame of a bar line in the pass: the first at or after it, so a
        // line before the pass is negative. Bars are counted by number, never
        // re-derived from a beat (`bar_at` of a line can name the bar before).
        let frame_of =
            |boundary: Beat| first_frame_at_or_after((boundary - pass.beat) / beats_per_sample);
        let first = meter.bar_at(pass.beat).bar;
        let mut bar = if frame_of(meter.bar_start(first)) >= 0 {
            first
        } else {
            first + BarCount::new(1)
        };
        let mut boundary = meter.bar_start(bar);
        while let Some(k) = usize::try_from(frame_of(boundary))
            .ok()
            .filter(|&k| k < pass.len.get())
        {
            let signature = meter.at(boundary);
            if messages.flex && self.flex.meter != Some(signature) {
                // A whole-note meter has no standard Set Time Signature: it is stated
                // by none (its Set Metronome, 96 clocks, is still sent below).
                if let Some(wire) = flex_time_signature_wire(signature) {
                    emit((
                        frame_offset(pass.start.get() + k, block_size),
                        Message::Flex(self.group, Flex::SetTimeSignature(wire)).into(),
                    ));
                }
                self.flex.meter = Some(signature);
            }
            if messages.metronome && self.flex.metronome != Some(signature) {
                // A meter with no Set Metronome form (a 64th-note beat) sends none, and
                // the receiver keeps the click it was last told.
                if let Some(metronome) = flex_metronome(signature) {
                    emit((
                        frame_offset(pass.start.get() + k, block_size),
                        Message::Flex(self.group, Flex::SetMetronome(metronome)).into(),
                    ));
                }
                self.flex.metronome = Some(signature);
            }
            bar += BarCount::new(1);
            let next = meter.bar_start(bar);
            // A line that does not advance would stand here for ever.
            if next <= boundary {
                break;
            }
            boundary = next;
        }
    }

    /// Emit MTC quarter-frames that fall within `pass`. Quarter-frames go
    /// out at `fps * 4` Hz; each carries one of 8 nibbles of the current SMPTE
    /// time (M2-104 §7.6, MTC quarter-frame 0xF1).
    fn tick_mtc(
        &mut self,
        pass: &ClockPass,
        block_size: usize,
        sample_rate: SampleRate,
        beats_per_sample: BeatDuration,
        emit: &mut impl FnMut(UmpAt),
    ) {
        let (beat, len) = (pass.beat, pass.len.get());
        let fps = self.fps;
        let rate = sample_rate;

        // Where the block starts on the grid segment, and the next
        // quarter-frame to send. Each one's frame is derived from its index
        // in closed form (`mtc_due`), never by adding a quarter-frame's
        // length to a carried phase, so the grid does not drift however
        // long it runs. An edge or locate restarts it at quarter-frame 0 on
        // the block's first frame: piece 0 first, which is what a decoder
        // needs to lock.
        let at = self.mtc.frames;
        let mut next = self.mtc.next;

        // The sample position of the block's first frame, from beat 0. A pass
        // with none sends no quarter-frame and leaves the grid restarted, to
        // align on the first pass that has one; the stretch counts one
        // refusal.
        let Some(block_pos) = samples_at(beat, beats_per_sample) else {
            if !self.mtc.skipped {
                self.refused += Losses::refused(1);
            }
            self.restart_mtc();
            self.mtc.skipped = true;
            return;
        };
        // A restarted grid puts its first piece 0 on the next even-frame
        // boundary of the timeline (a multiple of two frames' samples from
        // beat 0), so the label is the frame it goes out on. The grid then
        // runs in frames from there: exact at a steady tempo, while a tempo
        // change moves the timeline under a grid fixed in samples, and the
        // label (read from the beat at each piece 0) follows within a frame.
        if std::mem::take(&mut self.mtc.align) {
            let period = 2.0 * frame_span(fps, rate).get();
            let cycles = block_pos / period;
            let k = if (cycles - cycles.round()).abs() < 1e-6 {
                cycles.round()
            } else {
                cycles.ceil()
            };
            self.mtc.lead = (k * period - block_pos).max(0.0);
        }
        let mut cycle = self.mtc.cycle;
        let mut skipped = self.mtc.skipped;

        // Quarter-frames due inside [0, len), each on the first frame at or
        // after its time (`first_frame_at_or_after`, the rule the clock ticks
        // use): one due on a block boundary goes out once, on the next
        // block's first frame, and one already due goes out on frame 0.
        loop {
            let due = first_frame_at_or_after(self.mtc_due(next, rate, fps) - at as f64).max(0);
            let Some(qf_sample) = usize::try_from(due).ok().filter(|&k| k < len) else {
                break;
            };
            let sample_offset = pass.start.get() + qf_sample;
            let piece = (next & 0x07) as u8;
            // A cycle's timecode is that of the sample piece 0 goes out on;
            // the receiver adds the two frames the cycle takes.
            if piece == 0 {
                let position = mtc_cycle(
                    Seconds::new((block_pos + qf_sample as f64) / rate.get()),
                    fps,
                );
                skipped = position.is_none();
                if let Some(next_cycle) = position {
                    cycle = next_cycle;
                } else {
                    // One refusal for the whole cycle: nothing of it goes out.
                    self.refused += Losses::refused(1);
                }
            }
            if !skipped {
                emit((
                    frame_offset(sample_offset, block_size),
                    self.system(System::TimeCode(
                        cycle.frame(QuarterFramePiece::masked(piece)).data(),
                    )),
                ));
            }
            next += 1;
        }

        self.mtc.cycle = cycle;
        self.mtc.skipped = skipped;
        self.mtc.next = next;
        self.mtc.frames = at + len as u64;
    }
}

/// The frames of `pass`, in order, that a 24-PPQN tick falls on: the first
/// frame at or after beat `k / 24` (`first_frame_at_or_after`) for each tick
/// `k` the pass reaches, compared as integer frames so a tick on a block
/// boundary is reached on the next block's first frame, not both.
fn tick_frames(pass: &ClockPass, beats_per_sample: BeatDuration) -> impl Iterator<Item = usize> {
    let tick_beats = BeatDuration::new(1.0 / f64::from(System::TIMING_CLOCKS_PER_QUARTER));
    let (beat, len) = (pass.beat, pass.len.get());
    // The tick at or before the pass's first beat: its frame is 0 (on the
    // beat, within the rule's tolerance) or before the pass (already sent, or
    // behind an off-tick start).
    let mut tick_idx = (beat.get() / tick_beats.get()).floor() as i64;
    std::iter::from_fn(move || {
        loop {
            let tick_beat = Beat::new(tick_idx as f64 * tick_beats.get());
            let k = first_frame_at_or_after((tick_beat - beat) / beats_per_sample);
            tick_idx += 1;
            if k >= 0 {
                return usize::try_from(k).ok().filter(|&k| k < len);
            }
        }
    })
}

/// A frame index as an event's offset in a block of `block_size` frames. Every
/// frame placed is inside the block, so the clamp onto the last frame never
/// runs.
fn frame_offset(frames: usize, block_size: usize) -> Offset {
    Offset::new(
        Samples::new(frames.min(block_size.saturating_sub(1))),
        Samples::new(block_size),
    )
    .unwrap_or(Offset::ZERO)
}

/// The frames one MTC frame lasts at `rate`: the exact frame period, kept
/// fractional.
fn frame_span(fps: MtcRate, rate: SampleRate) -> SampleSpan {
    SampleSpan::from_seconds(fps.frame_rate().frame_period(), rate)
        .expect("a frame period and a sample rate are positive")
}

/// The sample position of `beat` from beat 0 at a steady `beats_per_sample`:
/// the timeline the MTC grid and the Full Message share. A beat before zero
/// (or not a number), or a tempo that does not advance, has none: MTC states
/// no time there.
fn samples_at(beat: Beat, beats_per_sample: BeatDuration) -> Option<f64> {
    if beats_per_sample <= BeatDuration::new(0.0) {
        return None;
    }
    let samples = beat.get() / beats_per_sample.get();
    (samples >= 0.0).then_some(samples)
}

/// The eight Quarter Frames of the cycle that starts at `position` from
/// transport beat 0: the label of the nearest frame, on an
/// even one (a cycle's eight pieces span two frames). Rounding, not flooring,
/// so a position a float's width short of a frame boundary still reads as that
/// frame.
///
/// `None` for a time before zero or NaN; the hours wrap at 24 as MTC's do.
fn mtc_cycle(position: Seconds, fps: MtcRate) -> Option<QuarterFrameCycle> {
    let nearest = SmpteTimecode::from_seconds_wrapping(position, fps.frame_rate())?;
    Some(MtcTimeCode::from_frame_count(nearest.frame_count() & !1, fps).cycle())
}

#[cfg(test)]
mod tests;
