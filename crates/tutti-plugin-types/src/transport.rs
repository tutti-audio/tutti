//! Transport state shared with the plugin each process block.
//!
//! Superset of the fields VST2, VST3, and CLAP plugins consume. Formats
//! that don't surface a given field leave it unread.
//!
//! Fields are grouped into focused sub-structs ([`TransportFlags`],
//! [`MusicalTiming`], [`TransportPosition`], [`LoopRegion`], [`BarInfo`])
//! so the top-level type stays readable; callers access via
//! `transport.state.playing`, `transport.timing.tempo`, etc.
//!
//! Every quantity a tutti unit covers is carried as that unit ([`SampleRate`],
//! [`Bpm`], [`Beat`], [`LoopRange`], [`Seconds`], [`TimeSignature`],
//! [`BarNumber`]): the snapshot crosses the IPC wire, where serde reads each unit
//! through its own range check, and a format host calls `.get()` (or its `From`)
//! where it fills the format's C struct. Positions are quarter notes, the engine's
//! beat: one [`Beat`] per concept, read by VST2, VST3, CLAP and AU alike.

use arrayvec::ArrayVec;
use tutti_graph::MAX_TRANSPORT_CHANGES;
use tutti_music::{LoopRange, TimeSignature};
use tutti_ump::System;
use tutti_units::{BarNumber, Beat, BeatDuration, Bpm, SampleRate, Seconds};

/// A transport that takes effect part-way into a chunk: from frame
/// [`offset`](Self::offset) of the chunk on, the transport is
/// [`transport`](Self::transport), until the next change or the chunk's end.
///
/// A chunk's own transport ([`TransportInfo`], sent beside it) is the one at
/// its first frame; a change is never at frame 0. A format with an
/// inter-block transport event (CLAP's `CLAP_EVENT_TRANSPORT`) sends one
/// event per change, at its frame. VST3, VST2 and AU read a chunk's first
/// transport alone: none has such an event.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportChange {
    /// The frame within the chunk, a raw `u32` as the wire's MIDI offset is:
    /// the types stop at the IPC boundary.
    pub offset: u32,
    /// The transport from that frame on.
    pub transport: TransportInfo,
}

/// The transport changes inside one chunk, in time order: at most the graph's
/// own bound per block ([`MAX_TRANSPORT_CHANGES`]).
/// Inline, so a chunk's payload never allocates one; a change that does not fit
/// is the producer's to count.
pub type TransportChanges = ArrayVec<TransportChange, MAX_TRANSPORT_CHANGES>;

/// A transport snapshot passed into a plugin's process call.
///
/// A snapshot is built for a rate ([`new`](Self::new)): stopped, at 120 BPM,
/// 4/4, at beat 0, with no cycle; the `with_*` builders fill in the rest.
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::{BarNumber, TransportInfo};
/// use tutti_units::{Beat, Bpm, SampleRate};
///
/// let info = TransportInfo::new(SampleRate::of(48_000.0))
///     .tempo(Bpm::of(128.0))
///     .playing(true)
///     .beat(Beat::new(16.0))
///     .bar(Beat::new(16.0), BarNumber::default());
/// assert!(info.state.playing);
/// assert_eq!(info.position.samples, None); // the producer reports it separately
/// ```
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportInfo {
    /// Playing / recording.
    pub state: TransportFlags,
    /// Tempo and time signature at the playhead.
    pub timing: MusicalTiming,
    /// Playhead position, in whichever coordinate systems the host fills.
    pub position: TransportPosition,
    /// The cycle, `Some` while the transport is cycling: every format's
    /// cycle-active flag is this being present, and its bounds are read only
    /// then.
    pub loop_region: Option<LoopRegion>,
    /// Where the current bar starts, and its number.
    pub bar: BarInfo,
    /// The rate the block renders at (vst3 `ProcessContext::sampleRate`).
    pub sample_rate: SampleRate,
    /// The host's monotonic clock at the snapshot's first frame, in
    /// nanoseconds on the OS's monotonic clock (`CLOCK_MONOTONIC`; macOS
    /// `CLOCK_UPTIME_RAW`, Core Audio's host time; Windows QPC), the clock
    /// plugins compare it against (vst3 `systemTime`; vst2 `nanoSeconds`).
    ///
    /// `None` when the producer has no clock that relates to the audio: an
    /// offline render runs faster or slower than real time, and a hand-built
    /// snapshot has none. A raw `i64` because that is the ABI's type and an
    /// exact integer count, as the ABI has it.
    pub system_time_ns: Option<i64>,
}

/// Playback and record flags.
///
/// Named `TransportFlags` after the plugin-SDK term for exactly this bundle —
/// CLAP and VST2 both call the field `flags`. Distinct from tutti-timeline's
/// `TransportState` *trait* (a live-timeline reader, which no plugin node
/// consumes: each reads its block's transport from the graph); this is the
/// wire snapshot.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportFlags {
    /// `true` while the transport is rolling.
    pub playing: bool,
    /// `true` while the host is capturing. Independent of
    /// [`playing`](Self::playing) — a punch-in armed but not yet rolling is
    /// recording without playing.
    pub recording: bool,
}

/// Tempo and time signature at the playhead.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MusicalTiming {
    /// Tempo at the playhead: finite and above zero, by the type.
    pub tempo: Bpm,
    /// How the tempo moves across the block this snapshot opens, in BPM per
    /// sample, until the next transport event: CLAP's `tempo_inc`. `0.0` for
    /// a constant tempo, a stopped transport, or a block the host cannot
    /// describe by one slope (a tempo point inside it). A raw `f64`: no unit
    /// covers a tempo's slope (`Bpm` is a tempo, and a difference of two
    /// tempos is not one). VST2, VST3 and AU have no such field and read
    /// [`tempo`](Self::tempo) alone.
    pub tempo_inc: f64,
    /// The signature in force at the playhead.
    ///
    /// A [`TimeSignature`] rather than a loose `(i32, i32)` pair: each format
    /// wants a different width (CLAP `u16`, VST2/VST3 `i32`), and casting at
    /// three separate boundaries invites `as u16` on a signed value, where a
    /// negative numerator becomes 65535. The conversions live on the type and
    /// validate on the way through.
    pub signature: TimeSignature,
}

impl Default for MusicalTiming {
    /// 120 BPM, 4/4 — the conventional musical defaults.
    fn default() -> Self {
        Self {
            tempo: Bpm::default(),
            tempo_inc: 0.0,
            signature: TimeSignature::default(),
        }
    }
}

/// The playhead position in several coordinate systems.
///
/// Hosts populate whichever the underlying plugin format understands; each
/// format host reads only the fields it needs.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportPosition {
    /// Sample-accurate project timeline position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`; AU `outCurrentSampleInTimeLine`). Jumps when the
    /// transport loops or relocates.
    ///
    /// The engine's plugin node keeps it as state, as CLAP's
    /// [`seconds`](Self::seconds) (the same time, in frames, rounded): it
    /// rolls by the frames the transport played, through tempo changes,
    /// holds while it is stopped (a stop, a start where it stopped and a
    /// loop edit continue it, inside one chunk too), and is re-seated from
    /// the playhead's beat at the tempo in force
    /// (`beats * 60 / tempo * sample_rate`) only at a jump: the first block,
    /// a seek (stopped or playing), a loop wrap, or a sample-rate change.
    /// The frames played between two reads come from the engine's frame
    /// count on its timeline segment, exact for one tempo change between
    /// them. The engine has no tempo map, so after a tempo change the two
    /// no longer agree; the rolled count is the time actually played.
    ///
    /// `None` means the producer has no project-time position to report (a
    /// non-finite playhead, or a hand-built snapshot). It is an `Option` rather than
    /// a plain `i64` because neither the VST2 nor the VST3 ABI has a validity
    /// bit for its sample-position field, so a placeholder `0` would travel
    /// as fact; the `Option` makes each format host decide what to send.
    pub samples: Option<i64>,
    /// Monotonic sample counter that does **not** reset on loop/cycle (vst3
    /// `continousTimeSamples`; clap `steady_time`). Free-running plugins (LFOs,
    /// delays) key their timing off this. A count from 0 at the instance's first
    /// block that never goes back: `0` is its first frame, never "unset", and a
    /// format host sends it as it is.
    pub continuous_samples: i64,
    /// The playhead in quarter notes from the song's start, the engine's beat
    /// (vst2 `ppqPos`; vst3 `projectTimeMusic`; clap `song_pos_beats`; AU
    /// `outCurrentBeat`).
    pub beat: Beat,
    /// Seconds from song start (clap `song_pos_seconds`): the engine's
    /// plugin node fills it as [`samples`](Self::samples) over the rate,
    /// unrounded. It is the only source of CLAP's seconds timeline: a host
    /// derives nothing from `samples` when it is zero. A host reads `.get()`
    /// where it hands the value to a C ABI.
    pub seconds: Seconds,
}

/// The cycle: its bounds and where they fall in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoopRegion {
    /// The cycle in quarter notes from the song's start (vst2 `cycleStartPos`,
    /// `cycleEndPos`; vst3 `cycleStartMusic`, `cycleEndMusic`; clap
    /// `loop_start_beats`, `loop_end_beats`; AU `outCycleStartBeat`,
    /// `outCycleEndBeat`).
    pub range: LoopRange,
    /// The cycle's `(start, end)` in seconds from song start (clap
    /// `loop_start_seconds`, `loop_end_seconds`), read from the project's tempo
    /// map at the beats above. [`None`] when the producer had no map to read:
    /// a consumer then claims no seconds timeline for the loop.
    pub seconds: Option<(Seconds, Seconds)>,
}

impl LoopRegion {
    /// A cycle over `range`, with no seconds timeline.
    #[must_use]
    pub fn new(range: LoopRange) -> Self {
        Self {
            range,
            seconds: None,
        }
    }
}

/// Where the current bar starts, and its number.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BarInfo {
    /// Where the current bar starts, in quarter notes (vst2/vst3
    /// `barPositionMusic`; clap `bar_start`; AU `outCurrentMeasureDownBeat`).
    pub start: Beat,
    /// The bar's number, **1-based** like every [`BarNumber`]. CLAP's
    /// `bar_number` is 0-based, so the CLAP host sends
    /// [`index`](BarNumber::index).
    pub number: BarNumber,
}

impl Default for BarInfo {
    /// The first bar, which starts at position 0.
    fn default() -> Self {
        Self {
            start: Beat::new(0.0),
            number: BarNumber::FIRST,
        }
    }
}

impl TransportInfo {
    /// A stopped transport at `sample_rate`: 120 BPM, 4/4, beat 0, no cycle.
    #[must_use]
    pub fn new(sample_rate: SampleRate) -> Self {
        Self {
            state: TransportFlags::default(),
            timing: MusicalTiming::default(),
            position: TransportPosition::default(),
            loop_region: None,
            bar: BarInfo::default(),
            sample_rate,
            system_time_ns: None,
        }
    }

    /// Sets the tempo (builder).
    #[must_use]
    pub fn tempo(mut self, tempo: Bpm) -> Self {
        self.timing.tempo = tempo;
        self
    }

    /// Sets the tempo's slope in BPM per sample (builder): CLAP's `tempo_inc`.
    #[must_use]
    pub fn tempo_inc(mut self, tempo_inc: f64) -> Self {
        self.timing.tempo_inc = tempo_inc;
        self
    }

    /// Sets the playing flag (builder).
    #[must_use]
    pub fn playing(mut self, playing: bool) -> Self {
        self.state.playing = playing;
        self
    }

    /// Sets the recording flag (builder). Independent of the playing flag.
    #[must_use]
    pub fn recording(mut self, recording: bool) -> Self {
        self.state.recording = recording;
        self
    }

    /// Sets the time signature at the playhead (builder).
    #[must_use]
    pub fn time_signature(mut self, signature: TimeSignature) -> Self {
        self.timing.signature = signature;
        self
    }

    /// Sets the playhead in quarter notes (builder).
    ///
    /// Does **not** set a sample position: the producer reports that
    /// separately via [`position_samples`](Self::position_samples),
    /// and one that has none leaves [`TransportPosition::samples`] `None`.
    #[must_use]
    pub fn beat(mut self, beat: Beat) -> Self {
        self.position.beat = beat;
        self
    }

    /// Sets the playhead in seconds from song start (builder).
    #[must_use]
    pub fn seconds(mut self, seconds: Seconds) -> Self {
        self.position.seconds = seconds;
        self
    }

    /// Sets the project-time sample position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`), the one that jumps on loop/relocate (builder).
    ///
    /// See [`TransportPosition::samples`] for how the engine derives it.
    #[must_use]
    pub fn position_samples(mut self, samples: i64) -> Self {
        self.position.samples = Some(samples);
        self
    }

    /// Sets the monotonic continuous sample counter (vst3
    /// `continousTimeSamples`, clap `steady_time`), the one that does not reset
    /// on loop (builder).
    ///
    /// Left at `0`, consumers fall back to the project-time `samples`.
    #[must_use]
    pub fn continuous_samples(mut self, continuous_samples: i64) -> Self {
        self.position.continuous_samples = continuous_samples;
        self
    }

    /// Sets the host's monotonic clock reading, in nanoseconds (builder). See
    /// [`TransportInfo::system_time_ns`].
    #[must_use]
    pub fn system_time_ns(mut self, nanoseconds: i64) -> Self {
        self.system_time_ns = Some(nanoseconds);
        self
    }

    /// Frames from the snapshot's first frame to the nearest MIDI clock, at
    /// 24 clocks per quarter note (vst2 `samplesToNextClock`, vst3
    /// `samplesToNextClock`): negative when the nearest clock is behind,
    /// as both SDKs define it ("can be negative (nearest)").
    ///
    /// Derived from the musical position, the tempo and the rate, so it is
    /// the grid the [`beat`](TransportPosition::beat) the plugin is told
    /// already implies. `None` unless the position is finite: the tempo and
    /// the rate are valid by their types, a [`Beat`] is not.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_plugin_types::TransportInfo;
    /// use tutti_units::{Beat, Bpm, SampleRate};
    ///
    /// // 120 BPM at 48 kHz: a quarter note is 24 000 frames, a clock 1 000.
    /// let t = TransportInfo::new(SampleRate::of(48_000.0))
    ///     .tempo(Bpm::of(120.0))
    ///     .beat(Beat::new(2.0 + 0.25 / 24.0));
    /// assert_eq!(t.samples_to_nearest_clock(), Some(-250));
    /// ```
    pub fn samples_to_nearest_clock(&self) -> Option<i32> {
        let clocks_per_quarter = f64::from(System::TIMING_CLOCKS_PER_QUARTER);
        let quarters = self.position.beat.get();
        if !quarters.is_finite() {
            return None;
        }
        let clocks = quarters * clocks_per_quarter;
        let to_nearest = (clocks.round() - clocks) / clocks_per_quarter;
        let frames_per_beat = self.timing.tempo.frames_per_beat(self.sample_rate)?;
        let frames = frames_per_beat
            .frames_in(BeatDuration::new(to_nearest))
            .round();
        // Half a clock at the slowest tempo a plugin is told is far inside
        // `i32`; a pathological tempo saturates rather than wraps.
        Some(frames.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
    }

    /// Frames from the snapshot's first frame to the next whole beat, as AU's
    /// `outDeltaSampleOffsetToNextBeat` wants it: `0` when the playhead is on a
    /// beat (within half a frame) and when the transport is stopped, since a
    /// stopped playhead never reaches the next one.
    ///
    /// Derived from [`position.beat`](TransportPosition::beat) at the
    /// snapshot's tempo and rate. `None` unless the position is finite: the
    /// tempo and the rate are valid by their types, a [`Beat`] is not.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_plugin_types::TransportInfo;
    /// use tutti_units::{Beat, Bpm, SampleRate};
    ///
    /// // 120 BPM at 48 kHz: a beat is 24 000 frames; a quarter beat is left.
    /// let t = TransportInfo::new(SampleRate::of(48_000.0))
    ///     .tempo(Bpm::of(120.0))
    ///     .playing(true)
    ///     .beat(Beat::new(2.75));
    /// assert_eq!(t.samples_to_next_beat(), Some(6_000));
    /// ```
    // A bare `u32`, not `Samples`: it is AU's `UInt32` ABI value, as
    // `samples_to_nearest_clock` is the VST ABI's `i32`; the unit types stop at
    // a C ABI.
    pub fn samples_to_next_beat(&self) -> Option<u32> {
        let beats = self.position.beat.get();
        if !beats.is_finite() {
            return None;
        }
        if !self.state.playing {
            return Some(0);
        }
        let frames_per_beat = self.timing.tempo.frames_per_beat(self.sample_rate)?;
        let frames = frames_per_beat
            .frames_in(Beat::new(beats.ceil()) - self.position.beat)
            .round();
        // Just past a beat, the distance to the next is a whole beat less a
        // sliver: that is "on the beat", not "a beat away".
        if frames >= frames_per_beat.get().round() {
            return Some(0);
        }
        Some(frames.clamp(0.0, f64::from(u32::MAX)) as u32)
    }

    /// Sets where the current bar starts and its number (builder).
    #[must_use]
    pub fn bar(mut self, start: Beat, number: BarNumber) -> Self {
        self.bar.start = start;
        self.bar.number = number;
        self
    }

    /// Sets the cycle (builder): the transport is cycling.
    #[must_use]
    pub fn loop_region(mut self, loop_region: LoopRegion) -> Self {
        self.loop_region = Some(loop_region);
        self
    }
}

/// Tells a format host whether the playhead moved other than by playing, from
/// one block's snapshot to the next.
///
/// Formats that pull transport (AU's `outTransportStateChanged`) need to say
/// "start, stop, or locate since you last asked" so a plugin's internal
/// sequencer resynchronizes instead of counting on from the old position. The
/// host's snapshots carry no such flag, so it is derived: a play/stop flip, or a
/// beat position that is not where the previous block's playing would have put
/// it (a seek, a loop wrap), or the first block after [`reset`](Self::reset).
///
/// Plain data, no allocation: [`observe`](Self::observe) is called on the audio
/// thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlayheadWatch {
    /// Whether any block was observed since construction or [`reset`](Self::reset).
    seen: bool,
    /// The last block's `playing`.
    playing: bool,
    /// Where the previous block's playing leaves the playhead; `None` when the
    /// position was not finite, in which case only a play/stop flip is
    /// detected.
    expected_beats: Option<f64>,
}

impl PlayheadWatch {
    /// Beats of disagreement below which a position is "where it should be".
    /// Tempo ramps move the playhead by far less than this per block; a seek
    /// that small is inaudible.
    const BEAT_TOLERANCE: f64 = 1e-3;

    /// A watch that has seen nothing: its first block reports a change.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the previous block: the next [`observe`](Self::observe) reports a
    /// change. Call after anything that breaks continuity outside the
    /// snapshots (a sample-rate change, a reset).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Records this block's snapshot (`frames` long) and returns whether the
    /// transport started, stopped or jumped since the previous block.
    pub fn observe(&mut self, transport: &TransportInfo, frames: u32) -> bool {
        let playing = transport.state.playing;
        let beats = transport.position.beat.get();
        let jumped = match self.expected_beats {
            // Agreement is tested and negated, not `>` tested: a NaN position
            // is a jump, not agreement.
            Some(expected) => {
                let agrees = (beats - expected).abs() <= Self::BEAT_TOLERANCE;
                !agrees
            }
            None => false,
        };
        let changed = !self.seen || playing != self.playing || jumped;

        let (tempo, rate) = (transport.timing.tempo.get(), transport.sample_rate.get());
        self.expected_beats = if !beats.is_finite() {
            None
        } else if !playing {
            Some(beats)
        } else {
            Some(beats + f64::from(frames) / rate * tempo / 60.0)
        };
        self.seen = true;
        self.playing = playing;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_48k() -> TransportInfo {
        TransportInfo::new(SampleRate::of(48_000.0))
    }

    /// A snapshot nobody reported a project-time position on carries `None`,
    /// not a `0` that VST2/VST3 would forward as fact; the builder that
    /// reports one sets it, and only that builder.
    #[test]
    fn position_samples_is_absent_until_a_host_reports_one() {
        assert_eq!(at_48k().position.samples, None);
        assert_eq!(at_48k().beat(Beat::new(4.0)).position.samples, None);
        assert_eq!(
            at_48k().position_samples(1_234).position.samples,
            Some(1_234)
        );
    }

    /// **The MIDI clock offset is the distance to the nearest 24-ppq clock,
    /// in frames, signed.** At 120 BPM and 48 kHz a clock is 1 000 frames;
    /// at 90 BPM and 44.1 kHz it is 1 225. A quarter of a clock past a clock
    /// is behind it (negative), three quarters is ahead of the next.
    #[test]
    fn samples_to_nearest_clock_is_the_signed_distance_on_the_24_ppq_grid() {
        let at = |tempo: f64, rate: f64, quarters: f64| {
            TransportInfo::new(SampleRate::of(rate))
                .tempo(Bpm::of(tempo))
                .beat(Beat::new(quarters))
                .samples_to_nearest_clock()
        };
        assert_eq!(at(120.0, 48_000.0, 2.0), Some(0));
        assert_eq!(at(120.0, 48_000.0, 2.0 + 0.25 / 24.0), Some(-250));
        assert_eq!(at(120.0, 48_000.0, 2.0 + 0.75 / 24.0), Some(250));
        assert_eq!(at(90.0, 44_100.0, 1.0 + 0.4 / 24.0), Some(-490));
        assert_eq!(at(90.0, 44_100.0, 1.0 + 0.6 / 24.0), Some(490));

        // A position that is not a number has no grid.
        assert_eq!(at(120.0, 48_000.0, f64::NAN), None);
        assert_eq!(at(120.0, 48_000.0, f64::INFINITY), None);
    }

    /// **The next-beat offset is the distance to the next whole beat, in
    /// frames; on a beat and while stopped it is zero.** At 120 BPM and 48 kHz
    /// a beat is 24 000 frames; at 90 BPM and 44.1 kHz it is 29 400.
    #[test]
    fn samples_to_next_beat_is_the_distance_to_the_next_whole_beat() {
        let at = |tempo: f64, rate: f64, beats: f64, playing: bool| {
            TransportInfo::new(SampleRate::of(rate))
                .tempo(Bpm::of(tempo))
                .playing(playing)
                .beat(Beat::new(beats))
                .samples_to_next_beat()
        };
        assert_eq!(at(120.0, 48_000.0, 2.25, true), Some(18_000));
        assert_eq!(at(120.0, 48_000.0, 2.75, true), Some(6_000));
        assert_eq!(at(90.0, 44_100.0, 1.5, true), Some(14_700));
        // On a beat, and a hair past it: no distance, not a whole beat.
        assert_eq!(at(120.0, 48_000.0, 3.0, true), Some(0));
        assert_eq!(at(120.0, 48_000.0, 3.0 + 1e-9, true), Some(0));
        // Stopped: it never gets there.
        assert_eq!(at(120.0, 48_000.0, 2.25, false), Some(0));

        assert_eq!(at(120.0, 48_000.0, f64::NAN, true), None);
    }

    fn block(playing: bool, beats: f64) -> TransportInfo {
        at_48k().playing(playing).beat(Beat::new(beats))
    }

    /// **A playhead that advances as playing predicts is not a change; the
    /// first block, a start, a stop, a seek and a loop wrap are.** 512 frames
    /// at 120 BPM / 48 kHz is `512 / 24 000` beats.
    #[test]
    fn a_playhead_watch_reports_starts_stops_and_jumps_only() {
        let step = 512.0 / 24_000.0;
        let mut w = PlayheadWatch::new();
        assert!(w.observe(&block(false, 0.0), 512), "first block");
        assert!(!w.observe(&block(false, 0.0), 512), "stopped and still");
        assert!(w.observe(&block(true, 0.0), 512), "start");
        for i in 1..=20 {
            assert!(
                !w.observe(&block(true, f64::from(i) * step), 512),
                "steady block {i}"
            );
        }
        assert!(w.observe(&block(true, 40.0), 512), "seek forward");
        assert!(!w.observe(&block(true, 40.0 + step), 512), "steady again");
        assert!(w.observe(&block(true, 8.0), 512), "loop wrap back");
        assert!(w.observe(&block(false, 8.0 + step), 512), "stop");
        // Stopped: the playhead is expected where it is.
        assert!(!w.observe(&block(false, 8.0 + step), 512), "stopped, still");
        assert!(w.observe(&block(false, 3.0), 512), "seek while stopped");
    }

    /// `reset` makes the next block a change, and a position that is not a
    /// number predicts nothing: only a start or stop is seen after it.
    #[test]
    fn a_playhead_watch_resets_and_degrades_without_a_position() {
        let mut w = PlayheadWatch::new();
        let step = 512.0 / 24_000.0;
        w.observe(&block(true, 0.0), 512);
        assert!(!w.observe(&block(true, step), 512));
        w.reset();
        // Exactly where playing would have put it: only the reset makes it a change.
        assert!(
            w.observe(&block(true, 2.0 * step), 512),
            "first block after reset"
        );

        let mut w = PlayheadWatch::new();
        w.observe(&block(true, f64::NAN), 512);
        assert!(
            !w.observe(&block(true, 0.02), 512),
            "no prediction, no jump"
        );
        assert!(w.observe(&block(false, 0.02), 512), "stop is still seen");
    }
}
