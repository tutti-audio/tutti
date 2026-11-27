//! Plain shared transport state — the values, not the decisions.
//!
//! Nothing here goes through the command queue and nothing can reject a
//! write: setting the tempo is a store, not a request. That is the whole
//! distinction from [`MotionFsm`](super::MotionFsm), where the state machine
//! decides whether a transition happens at all.
//!
//! Each field is a lock-free cell shared between the UI thread and the audio
//! thread.

use super::ProjectMeter;
use super::TempoReader;
use super::state::{AtomicBpm, ClockLinks, PendingSeek};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tutti_music::MeterMap;
use tutti_rt::{Publisher, Reader};
use tutti_tempo::TempoMap;

use atomic_float::AtomicF64;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
use tutti_units::{Beat, Bpm};

/// Transport values shared between threads.
///
/// Clone shares every field — this is a handle, not a snapshot.
#[derive(Clone, Debug)]
pub struct TransportSettings {
    /// The project's tempo map, published by the control side
    /// ([`set_tempo_map`](Self::set_tempo_map), [`set_tempo`](Self::set_tempo))
    /// and lent to the engine's clock once per block through the one `Reader`
    /// its [`TransportAudio`](super::TransportAudio) registers. The session's
    /// `Reclaim` phase sweeps it ([`reclaim_tempo`](Self::reclaim_tempo)).
    pub(crate) tempo_map: Publisher<TempoMap>,
    /// Moves on after every publish to [`tempo_map`](Self::tempo_map), which is
    /// stored first: a clock that sees a new count lends the new map or a later
    /// one, never an older one.
    pub(crate) map_generation: Arc<AtomicU64>,
    /// The playhead, in beats. Written by `TransportClock` via its position
    /// writeback (and by a locate, which moves the readout at once); read by
    /// pull-based sources through [`beat`](Self::beat) (an app reads the
    /// session's playhead).
    ///
    /// Crate-private, as is [`segment_generation`](Self::segment_generation):
    /// only the transport's one audio side
    /// ([`Transport::audio`](super::Transport::audio)) stores them. A public
    /// cell was a second writer anyone could be:
    ///
    /// ```compile_fail,E0616
    /// let t = tutti_timeline::Transport::new(48_000.0);
    /// t.settings().beat.store(8.0, std::sync::atomic::Ordering::Release);
    /// ```
    ///
    /// A host that drives the playhead by hand takes the audio side and
    /// stores through it ([`TransportAudio::set_playhead`](super::TransportAudio::set_playhead)).
    pub(crate) beat: Arc<AtomicF64>,
    /// The segment the playhead is on
    /// ([`Timeline::segment_generation`](super::Timeline::segment_generation)):
    /// moves on at every seek, tempo or rate change, loop wrap and play
    /// start. Written by `TransportClock` beside [`beat`](Self::beat), and
    /// before it. Crate-private, like [`beat`](Self::beat):
    ///
    /// ```compile_fail,E0616
    /// let t = tutti_timeline::Transport::new(48_000.0);
    /// t.settings().segment_generation.store(0, std::sync::atomic::Ordering::Release);
    /// ```
    pub(crate) segment_generation: Arc<AtomicU64>,
    /// Derived from motion by the engine's motion machine — the clock
    /// reads it to decide whether to advance. Crate-private: nothing outside
    /// can set it.
    ///
    /// ```compile_fail,E0616
    /// let t = tutti_timeline::Transport::new(48_000.0);
    /// t.settings().paused.store(false, std::sync::atomic::Ordering::Release);
    /// ```
    pub(crate) paused: Arc<AtomicBool>,
    /// Free-running sample count since the stream started, written by
    /// `TransportClock` every buffer.
    ///
    /// Unlike [`beat`](Self::beat) this never jumps: it ignores loops, seeks and
    /// stops. Hosted plugins receive it as VST3's `continousTimeSamples` /
    /// CLAP's `steady_time`, which free-running effects key off precisely
    /// because the playhead is discontinuous.
    pub(crate) steady_time: Arc<AtomicI64>,
    /// The tempo the clock is actually running at: the map's, as the clock
    /// last took it, which for a constant map ignores wiggles smaller than its
    /// hysteresis. Written by the clock; what a beat must be resolved with
    /// to agree with it.
    pub(crate) current_tempo: Arc<AtomicBpm>,
    /// The project's meter: what the metronome and every hosted plugin read
    /// once per block. Owned here, beside the tempo map, so the transport is
    /// the one place the project's tempo and meter are held
    /// ([`project_meter`](Self::project_meter)).
    pub(crate) meter: ProjectMeter,
}

impl TransportSettings {
    /// Fresh settings: 120 BPM, beat 0, paused.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tempo_map: Publisher::new(Self::default_map()),
            map_generation: Arc::new(AtomicU64::new(0)),
            beat: Arc::new(AtomicF64::new(0.0)),
            segment_generation: Arc::new(AtomicU64::new(0)),
            paused: Arc::new(AtomicBool::new(true)),
            steady_time: Arc::new(AtomicI64::new(0)),
            current_tempo: Arc::new(AtomicBpm::new(Bpm::default())),
            meter: ProjectMeter::default(),
        }
    }

    /// The project's meter, 4/4 from bar 0 until one is published. A handle:
    /// a clone is the same meter, so the metronome's node, each hosted
    /// plugin's slot and the control side all follow what it publishes.
    #[must_use]
    pub fn project_meter(&self) -> &ProjectMeter {
        &self.meter
    }

    /// The meter as published now. Control thread: an owning handle, as
    /// [`ProjectMeter::current`] gives.
    #[must_use]
    pub fn meter_map(&self) -> Arc<MeterMap> {
        self.meter.current()
    }

    /// A reader of the project's meter, registered here on the control side
    /// ([`ProjectMeter::reader`]), for the one engine that lends it once per
    /// block.
    #[must_use]
    pub fn meter_reader(&self) -> Reader<MeterMap> {
        self.meter.reader()
    }

    /// Frees the meters no block holds any more, and returns how many are still
    /// held ([`ProjectMeter::reclaim`]). Control thread, on a tick it already
    /// has: the session's `Reclaim` phase.
    pub fn reclaim_meter(&self) -> usize {
        self.meter.reclaim()
    }

    /// The tempo the clock is actually running at (see the field).
    #[must_use]
    pub fn current_tempo(&self) -> Bpm {
        self.current_tempo.load(Ordering::Acquire)
    }

    /// Samples elapsed since the stream started. Free-running: never reset by a
    /// loop, seek, or stop.
    pub fn steady_time(&self) -> i64 {
        self.steady_time.load(Ordering::Relaxed)
    }

    /// Sets a constant tempo from the next block: sugar for publishing
    /// `TempoMap::constant(bpm)` ([`set_tempo_map`](Self::set_tempo_map)), so it
    /// **replaces** any map. A [`Bpm`] is finite and above zero, so a caller
    /// holding a raw number checks it with [`Bpm::new`] first.
    ///
    /// A tempo so small that a beat's rate underflows (a denormal) is no map
    /// ([`TempoMap::constant`] refuses it): nothing is published and the
    /// project keeps the map it had.
    pub fn set_tempo(&self, bpm: Bpm) {
        if let Ok(map) = TempoMap::constant(bpm) {
            self.tempo_map.publish(map);
        }
        self.map_generation.fetch_add(1, Ordering::Release);
    }

    /// Publishes `map` as the project's tempo: the engine takes it at the next
    /// block start.
    ///
    /// Any map is followed: the clock takes each step on the frame it falls
    /// on and each ramp cell by cell, a constant tempo of
    /// 128 frames at the chord of the curve.
    pub fn set_tempo_map(&self, map: impl Into<Arc<TempoMap>>) {
        self.tempo_map.publish(map.into());
        self.map_generation.fetch_add(1, Ordering::Release);
    }

    /// The map as published now. Control thread: it takes the cell's lock and
    /// an owning handle, as [`Publisher::current`] does.
    #[must_use]
    pub fn tempo_map(&self) -> Arc<TempoMap> {
        self.tempo_map.current()
    }

    /// Frees the maps no block holds any more, and returns how many are still
    /// held. Control thread, on a tick it already has: the session's `Reclaim`
    /// phase.
    pub fn reclaim_tempo(&self) -> usize {
        self.tempo_map.reclaim()
    }

    fn default_map() -> TempoMap {
        TempoMap::constant(Bpm::default()).expect("120 BPM is a map")
    }

    /// A reader of the map, registered here on the control side: for the one
    /// engine that lends it once per block, or a hosted node that reports the
    /// project's tempo ([`TempoReader`]). Each call registers a reader of its
    /// own.
    #[must_use]
    pub fn tempo_reader(&self) -> TempoReader {
        TempoReader::new(&self.tempo_map)
    }

    /// The playhead as last published by the clock: the transport's own read
    /// (its [`Timeline`](super::Timeline)). An app reads the session's
    /// playhead, which carries the beat with the play state and the frame.
    pub(crate) fn beat(&self) -> Beat {
        Beat::new(self.beat.load(Ordering::Acquire))
    }

    /// The playhead's segment generation as last published by the clock.
    pub fn segment_generation(&self) -> u64 {
        self.segment_generation.load(Ordering::Acquire)
    }

    /// A locate moves the published playhead at once, ahead of the clock's
    /// own writeback. Outside this crate, the writer is
    /// [`TransportAudio::set_playhead`](super::TransportAudio::set_playhead).
    pub(crate) fn locate(&self, beat: impl Into<Beat>) {
        self.beat.store(beat.into().get(), Ordering::Release);
    }

    /// The clock holds position. Called by the motion machine, the one writer
    /// of the flag.
    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// The clock advances. Called by the motion machine, the one writer of
    /// the flag.
    pub(crate) fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    /// The links a clock over these settings shares, writer half included,
    /// with `seek` as the pending jump it consumes. Crate-private: the
    /// transport's one audio side is the only caller.
    pub(crate) fn clock_links(&self, seek: PendingSeek) -> ClockLinks {
        ClockLinks {
            paused: Arc::clone(&self.paused),
            seek,
            position_writeback: Some(Arc::clone(&self.beat)),
            segment_generation: Some(Arc::clone(&self.segment_generation)),
            steady_time: Some(Arc::clone(&self.steady_time)),
            current_tempo: Some(Arc::clone(&self.current_tempo)),
        }
    }

    /// Whether the clock is holding position.
    ///
    /// Read-only here: the flag is derived from motion by
    /// the engine's motion machine, so there is no setter to race it with.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
}

impl Default for TransportSettings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_shares_state() {
        let a = TransportSettings::new();
        let b = a.clone();

        a.set_tempo(Bpm::of(140.0));
        assert_eq!(
            b.tempo_map().constant_tempo(),
            Some(Bpm::of(140.0)),
            "clones must share, not copy"
        );
    }

    /// `set_tempo` is the constant map of that tempo, to the bit, and the
    /// beats fall where the definition puts them: a beat is `60 / bpm`
    /// seconds, so beat 4 at 48 kHz is `4 x 60 x 48 000 / bpm` frames.
    #[test]
    fn set_tempo_publishes_the_constant_map_of_that_tempo() {
        let rate = tutti_units::SampleRate::SR_48K;
        let s = TransportSettings::new();
        for bpm in [120.0, 90.0, 97.123_456_789_012_35, 120.587_389_506_707_47] {
            s.set_tempo(Bpm::of(bpm));
            let map = s.tempo_map();
            assert_eq!(map.constant_tempo(), Some(Bpm::of(bpm)), "{bpm} to the bit");
            let frames = 4.0 * 60.0 * 48_000.0 / bpm;
            assert_eq!(
                map.view(rate).frame_at_or_after(Beat::new(4.0)).get(),
                tutti_music::first_frame_at_or_after(frames) as u64,
                "beat 4 at {bpm}"
            );
        }
    }

    /// A constant map, a map of steps and a map with a ramp are each the
    /// taken whole.
    #[test]
    fn a_constant_a_step_and_a_ramp_map_are_each_taken() {
        use tutti_tempo::{TempoPoint, Transition};
        let s = TransportSettings::new();
        let constant = TempoMap::constant(Bpm::of(75.5)).unwrap();
        s.set_tempo_map(Arc::new(constant));
        assert_eq!(s.tempo_map().constant_tempo(), Some(Bpm::of(75.5)));

        let two = |shape| {
            TempoMap::new([
                TempoPoint {
                    beat: Beat::new(0.0),
                    tempo: Bpm::of(100.0),
                    transition: shape,
                },
                TempoPoint {
                    beat: Beat::new(4.0),
                    tempo: Bpm::of(200.0),
                    transition: Transition::Step,
                },
            ])
            .unwrap()
        };
        for shape in [
            Transition::RampTime { end: None },
            Transition::RampBeats { end: None },
        ] {
            s.set_tempo_map(Arc::new(constant_of(75.5)));
            s.set_tempo_map(Arc::new(two(shape)));
            assert_eq!(s.tempo_map().constant_tempo(), None);
            assert_eq!(s.tempo_map().points()[0].transition, shape);
        }

        s.set_tempo_map(Arc::new(two(Transition::Step)));
        assert_eq!(s.tempo_map().constant_tempo(), None);
    }

    fn constant_of(bpm: f64) -> TempoMap {
        TempoMap::constant(Bpm::of(bpm)).unwrap()
    }

    /// A map a block holds when it is replaced stays retired until the sweep
    /// after the block, then is freed: `reclaim_tempo` is that sweep.
    #[test]
    fn the_tempo_map_is_swept_by_reclaim_tempo() {
        let s = TransportSettings::new();
        let mut reader = s.tempo_reader();
        let held = reader.lend();
        let old = Arc::downgrade(&s.tempo_map());
        s.set_tempo(Bpm::of(90.0));
        assert!(old.upgrade().is_some(), "a block still reads it");
        assert_eq!(s.reclaim_tempo(), 1, "pinned by the loan");
        drop(held);
        assert_eq!(s.reclaim_tempo(), 0);
        assert!(old.upgrade().is_none(), "freed on the control side");
    }
}
