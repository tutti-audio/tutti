//! Offline transport — a simulated [`super::Timeline`] that advances
//! deterministically by sample count rather than wall clock.
//!
//! Primary consumer is offline audio export, but anything that needs a
//! reproducible timeline without a real CPAL callback (tests,
//! automation scrubbing) can use it.

use std::sync::{Arc, MutexGuard};
use tutti_rt::Mutex;
use tutti_tempo::TempoMap;

use super::walk::TempoWalk;
use atomic_float::AtomicF64;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_music::LoopRange;
use tutti_music::{FrameClock, MeterMap};
use tutti_units::{Beat, Bpm, SampleRate, Samples};

/// Configuration for constructing an [`OfflineTimeline`].
#[derive(Debug, Clone)]
pub struct OfflineTimelineConfig {
    /// Start position in beats.
    pub start_beat: Beat,
    /// The tempo map the timeline runs on: the project's tempo
    /// (`TransportSettings::tempo_map`). The timeline follows its steps, each
    /// on the frame it falls on, and its ramps cell by cell, as the live
    /// transport does, and it keeps the map it was given: a later edit to the
    /// project's map does not reach it.
    pub tempo_map: Arc<TempoMap>,
    /// Sample rate in Hz.
    pub sample_rate: SampleRate,
    /// Loop region, if looping. Already validated — build one with
    /// [`LoopRange::new`], which rejects empty and inverted regions.
    pub loop_range: Option<LoopRange>,
}

impl Default for OfflineTimelineConfig {
    fn default() -> Self {
        Self {
            start_beat: Beat::new(0.0),
            tempo_map: Arc::new(TempoMap::constant(Bpm::default()).expect("120 BPM is a map")),
            sample_rate: SampleRate::SR_44K1,
            loop_range: None,
        }
    }
}

/// Simulated transport that advances by sample count.
///
/// Implements [`Timeline`](super::Timeline), so any node that accepts a
/// `&dyn Timeline` treats it interchangeably with the live
/// [`Transport`](super::Transport).
///
/// # Example
/// ```
/// # use std::sync::Arc;
/// # use tutti_tempo::TempoMap;
/// # use tutti_units::{Beat, Bpm, SampleRate, Samples};
/// # use tutti_timeline::Timeline;
/// # use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};
/// let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
///     start_beat: Beat::new(0.0),
///     tempo_map: Arc::new(TempoMap::constant(Bpm::of(120.0))?),
///     sample_rate: SampleRate::SR_44K1,
///     loop_range: None,
/// });
///
/// // Advance by 44100 samples (1 second at 44.1kHz)
/// // At 120 BPM, that's 2 beats
/// timeline.advance(Samples::new(44100));
/// assert!((timeline.beat().get() - 2.0).abs() < 0.001);
/// # Ok::<_, tutti_tempo::TempoError>(())
/// ```
///
/// # The frame is the source of truth
///
/// The playhead is a frame count on a segment, and the beat is
/// derived from it in closed form, never accumulated: at 90 BPM and 48 kHz,
/// 1 500 blocks of 64 frames is beat 3 to the bit, where adding
/// `beats_per_sample × 64` per block reads `2.999999999999891` and a clip
/// placed at beat 3 enters a block late. A loop wrap starts a new segment on
/// the frame that reaches the loop's end, as the live `TransportClock` does,
/// so the two agree to the bit however each is stepped.
///
/// # Shared vs. fixed state
///
/// Only the position is shared: `advance`/`seek_to` take `&self` because the
/// timeline is held as an `Arc` and read by clip readers and samplers while
/// the export driver advances it. Everything else is render configuration
/// fixed at construction — there are no setters — so it is a plain value.
///
/// The position is a [`FrameClock`] behind a `Mutex`: a move (`advance`,
/// `seek_to`) takes the lock, moves the clock, and publishes the beat it
/// derives before letting go, so two writers racing (a seek against an
/// advance) serialise, and neither can publish a position the other half
/// wrote. Readers read the published beat alone, lock-free. An offline
/// render, not the audio thread, is what takes the lock.
///
/// The alignment keeps the published beat — the one genuinely contended word
/// — on its own cache line, away from the immutable fields readers also
/// touch.
#[derive(Debug)]
#[repr(align(64))]
pub struct OfflineTimeline {
    /// The beat `clock` derives, published on every move: what readers
    /// read.
    current_beat: AtomicF64,
    /// The generation of `clock`'s segment, published with the beat
    /// (before it, as [`Timeline::segment_generation`](super::Timeline::segment_generation)
    /// asks): a seek, even to the beat already there, and a loop wrap move
    /// it on.
    current_generation: AtomicU64,
    /// The playhead: the only writer-side state, moved under its lock.
    clock: Mutex<Head>,
    /// The tempo map the timeline was given.
    tempo_map: Arc<TempoMap>,
    /// Whether the map changes tempo: the playhead then follows it. A
    /// constant map runs the closed form of a [`FrameClock`] alone.
    stepped: bool,
    sample_rate: SampleRate,
    /// The active loop region, validated at construction — so `advance()` needs
    /// no `end > start` guard of its own.
    loop_range: Option<LoopRange>,
    /// The meter a render under this timeline lends its nodes
    /// ([`RenderClock::meter`](super::RenderClock::meter)), or `None` for a
    /// render with no meter (a node then counts 4/4 bars).
    meter: Option<Arc<MeterMap>>,
}

/// The playhead, and where it is on a map that changes tempo.
#[derive(Debug)]
struct Head {
    clock: FrameClock,
    walk: TempoWalk,
}

impl OfflineTimeline {
    /// Creates a timeline seated at `config.start_beat`, on `config.tempo_map`.
    ///
    /// A render that must see the map's tempo changes drives it through
    /// [`render_graph`](super::render_graph), which
    /// knows the block it hands the graph. A map of one point is the constant
    /// tempo, bit for bit.
    #[must_use]
    pub fn new(config: &OfflineTimelineConfig) -> Self {
        let map = &config.tempo_map;
        let mut clock =
            FrameClock::new(config.start_beat, map.points()[0].tempo, config.sample_rate);
        let mut walk = TempoWalk::default();
        let stepped = map.constant_tempo().is_none();
        if stepped {
            walk.sync_tempo(map, &mut clock);
        }
        Self {
            current_beat: AtomicF64::new(config.start_beat.get()),
            current_generation: AtomicU64::new(clock.generation()),
            clock: Mutex::new(Head { clock, walk }),
            tempo_map: Arc::clone(map),
            stepped,
            sample_rate: config.sample_rate,
            loop_range: config.loop_range,
            meter: None,
        }
    }

    /// This timeline lending `meter` to the graph it renders
    /// ([`RenderClock::meter`](super::RenderClock::meter)), so a metronome in
    /// the render counts the project's bars. It keeps the map it is given: a
    /// later edit to the project's meter does not reach it.
    #[must_use]
    pub fn meter(self, meter: Arc<MeterMap>) -> Self {
        Self {
            meter: Some(meter),
            ..self
        }
    }

    /// Advances the playhead by `samples` frames of render.
    ///
    /// If a loop region is set and the timeline crosses its end, the position
    /// wraps back into the region, on the frame that reaches the end.
    ///
    /// Counts frames and derives the beat in closed form (the type docs), with
    /// the live `TransportClock`'s own code, so the two land on the same beat
    /// to the bit whether either is stepped a frame or a block at a time —
    /// see the sample-for-sample test below.
    pub fn advance(&self, samples: Samples) {
        let mut head = self.lock();
        let Head { clock, walk } = &mut *head;
        // `LoopRange` is non-empty by construction, so no guard. Only a
        // crossing wraps, as for the clock (`LoopRange::advance`).
        if self.stepped {
            walk.advance(
                &self.tempo_map,
                clock,
                samples.get(),
                self.loop_range,
                false,
            );
        } else {
            clock.advance(samples, self.loop_range);
        }
        self.publish(&head.clock);
    }

    /// The playhead, for moving: held until the moved position is published.
    fn lock(&self) -> MutexGuard<'_, Head> {
        // A panic mid-move leaves a whole `FrameClock` (it is `Copy` and
        // moved by value), so a poisoned lock still holds a position.
        self.clock.lock()
    }

    /// Publish `clock`'s beat, and its segment's generation, to readers.
    /// Called with the lock held. The generation first: a reader that reads
    /// the beat, then the generation, never pairs this beat with an older
    /// generation (the order `Timeline::segment_generation` documents).
    fn publish(&self, clock: &FrameClock) {
        self.current_generation
            .store(clock.generation(), Ordering::Release);
        self.current_beat
            .store(clock.beat().get(), Ordering::Release);
    }

    /// The current playhead.
    #[inline]
    pub fn beat(&self) -> Beat {
        Beat::new(self.current_beat.load(Ordering::Acquire))
    }

    /// The render sample rate. Fixed at construction.
    #[inline]
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Moves the playhead to `beat`, for reusing one timeline across several
    /// renders.
    ///
    /// Named for the seek, not for a reset: it clears nothing, and `beat` is a
    /// destination rather than a default. Everything else on this timeline —
    /// tempo map, sample rate, loop region — is fixed at construction.
    pub fn seek_to(&self, beat: impl Into<Beat>) {
        let mut head = self.lock();
        let Head { clock, walk } = &mut *head;
        if self.stepped {
            walk.seek(&self.tempo_map, clock, beat.into());
        } else {
            clock.seat(beat.into());
        }
        self.publish(&head.clock);
    }

    /// The active loop region, or `None` when not looping.
    ///
    /// An inherent method, not a [`Timeline`](super::Timeline) one: the offline
    /// render never reads it through a trait — `advance()` folds the wrap in
    /// directly.
    #[inline]
    pub fn loop_range(&self) -> Option<LoopRange> {
        self.loop_range
    }

    /// The transport of `clock`'s frame: rolling, at the tempo and segment the
    /// clock stands on, looping over this timeline's region.
    fn transport(&self, clock: &FrameClock) -> tutti_graph::TransportPosition {
        tutti_graph::TransportPosition::counted(
            true,
            clock.tempo(),
            clock.origin(),
            self.loop_range,
        )
    }

    /// The next block of at most `frames` frames as the graph takes it: its
    /// transport, the tempo changes inside it, and how many frames it covers.
    /// That is `frames` unless more changes fall inside than a block holds
    /// ([`MAX_TRANSPORT_CHANGES`](tutti_graph::MAX_TRANSPORT_CHANGES)), when
    /// the block ends where the one that does not fit begins and the render
    /// continues from there: the cut is exact because
    /// the walk is a function of the frame, not of where blocks begin.
    fn block(
        &self,
        frames: usize,
    ) -> (
        tutti_graph::TransportPosition,
        tutti_graph::TransportChanges,
        usize,
    ) {
        let head = self.lock();
        let first = self.transport(&head.clock);
        if !self.stepped {
            return (first, tutti_graph::TransportChanges::NONE, frames);
        }
        let ahead = head.walk.ahead(
            &self.tempo_map,
            &head.clock,
            frames,
            self.loop_range,
            tutti_graph::MAX_TRANSPORT_CHANGES,
            |clock| self.transport(clock),
        );
        (first, ahead.changes, ahead.limit.unwrap_or(frames))
    }
}

impl super::Timeline for OfflineTimeline {
    fn beat(&self) -> Beat {
        self.beat()
    }

    fn tempo_map(&self) -> Arc<TempoMap> {
        Arc::clone(&self.tempo_map)
    }

    fn is_rolling(&self) -> bool {
        // An offline timeline advances whenever asked — there is nothing to
        // pause it.
        true
    }

    fn segment_generation(&self) -> u64 {
        self.current_generation.load(Ordering::Acquire)
    }
}

/// Only a render advances it: the offline context a fork is handed
/// ([`OfflineTransport::new`](super::OfflineTransport::new)).
///
/// The live transport is not one, so it cannot be passed as that context:
///
/// ```compile_fail,E0277
/// use std::sync::Arc;
/// use tutti_timeline::{OfflineTransport, Transport};
/// let live = Arc::new(Transport::new(48_000.0));
/// let ctx = OfflineTransport::new(live); // the live playhead as a render's: refused
/// ```
///
/// ```
/// use std::sync::Arc;
/// use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
/// let ctx = OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig::default())));
/// ```
impl super::OfflineClock for OfflineTimeline {}

impl super::RenderClock for OfflineTimeline {
    fn advance(&self, frames: tutti_units::Samples) {
        OfflineTimeline::advance(self, frames);
    }

    /// This timeline at its current playhead: rolling (an offline render always
    /// is), at its tempo, looping over its region, and counted from its
    /// segment's origin, so a node walking its `Env` continues this timeline's
    /// arithmetic. There are never changes here. A loop wrap inside the block
    /// is not a change: the graph derives it from the snapshot, as it does
    /// live, through [`Env::transport_at`](tutti_graph::Env::transport_at) and
    /// [`Env::for_each_beat`](tutti_graph::Env::for_each_beat). A tempo change
    /// of the map is cut by [`render_graph`](super::render_graph),
    /// which takes the block's changes from the map.
    fn graph_block(
        &self,
    ) -> (
        tutti_graph::TransportPosition,
        tutti_graph::TransportChanges,
    ) {
        let head = self.lock();
        (
            self.transport(&head.clock),
            tutti_graph::TransportChanges::NONE,
        )
    }

    /// The block's first piece, with its tempo steps in its changes, cut where
    /// more changes fall than a block holds
    /// ([`render_graph`](super::render_graph) asks again for the
    /// rest). Without a map with steps it is the whole block, as the default.
    fn graph_piece(
        &self,
        frames: usize,
    ) -> (
        tutti_graph::TransportPosition,
        tutti_graph::TransportChanges,
        usize,
    ) {
        self.block(frames)
    }

    fn meter(&self) -> Option<&MeterMap> {
        self.meter.as_deref()
    }

    /// The map the timeline was given: its render's nodes place by it.
    fn tempo(&self) -> Option<&TempoMap> {
        Some(&self.tempo_map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderClock;
    use tutti_buffers::Float;
    use tutti_tempo::Transition;

    fn constant(bpm: f64) -> Arc<TempoMap> {
        Arc::new(TempoMap::constant(Bpm::of(bpm)).expect("a tempo"))
    }

    /// An inverted region must not arm a loop that never wraps.
    ///
    /// Carried as an unvalidated pair plus an `enabled` flag, `end < start`
    /// arms the loop and then silently never wraps — the render runs straight
    /// past the loop end with no diagnostic. `Option<LoopRange>` makes that
    /// state unrepresentable: it is rejected at the boundary and the timeline
    /// is honestly un-looped.
    #[test]
    fn an_inverted_loop_region_is_rejected_not_silently_ignored() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(0.0),
            tempo_map: constant(120.0),
            sample_rate: SampleRate::SR_44K1,
            loop_range: LoopRange::new(Beat::new(8.0), Beat::new(4.0)),
        });

        assert_eq!(
            timeline.loop_range(),
            None,
            "an inverted region must not report as an active loop"
        );

        // ...and the timeline runs free rather than pretending to loop.
        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance(Samples::new((10.0 * samples_per_beat) as usize));
        assert!(
            (timeline.beat().get() - 10.0).abs() < 0.01,
            "expected free-running beat 10.0, got {}",
            timeline.beat().get()
        );
    }

    /// The empty region — the case a `loop_length > 0.0` guard inside `advance`
    /// would catch. `LoopRange` catches it one layer earlier, at construction.
    #[test]
    fn an_empty_loop_region_is_rejected() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(0.0),
            tempo_map: constant(120.0),
            sample_rate: SampleRate::SR_44K1,
            loop_range: LoopRange::new(Beat::new(4.0), Beat::new(4.0)),
        });
        assert_eq!(timeline.loop_range(), None);
    }

    /// A block longer than the loop lands where its frames do: inside the
    /// loop, exactly on the start when the crossings are on frames, and bit
    /// for bit where the same frames one at a time land when they are not.
    ///
    /// `advance` counts frames and starts a new segment on every crossing's
    /// frame (`FrameClock::advance`), as the live clock does frame by frame,
    /// so the two agree to the bit.
    #[test]
    fn a_block_longer_than_the_loop_lands_where_its_frames_do() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(0.0),
            tempo_map: constant(120.0),
            sample_rate: SampleRate::SR_44K1,
            loop_range: LoopRange::new(Beat::new(0.0), Beat::new(1.0)),
        });

        // Five beats over a one-beat loop: a crossing every 22 050 frames, each
        // on a frame (one beat is exactly 22 050), so each wraps to 0.0.
        let samples_per_beat = 22050usize;
        timeline.advance(Samples::new(5 * samples_per_beat));
        assert_eq!(
            timeline.beat(),
            Beat::new(0.0),
            "5 beats over a 1-beat loop land exactly on the start"
        );

        // Crossings between frames, at a tempo whose frame step is not
        // representable: a block at once and its frames one by one agree.
        let config = OfflineTimelineConfig {
            start_beat: Beat::new(0.1),
            tempo_map: constant(97.0),
            sample_rate: SampleRate::SR_44K1,
            loop_range: LoopRange::new(Beat::new(0.1), Beat::new(0.43)),
        };
        let (block, frames) = (OfflineTimeline::new(&config), OfflineTimeline::new(&config));
        block.advance(Samples::new(50_000));
        for _ in 0..50_000 {
            frames.advance(Samples::new(1));
        }
        assert!(config.loop_range.expect("a loop").contains(block.beat()));
        assert_eq!(block.beat().get().to_bits(), frames.beat().get().to_bits());
    }

    /// A render drives two clocks over one timeline: a `TransportClock` (the
    /// engine's playhead, whose transport each graph block's `Env` reads)
    /// while this `OfflineTimeline` feeds clip readers and
    /// samplers. Started at the same beat, they must report the same beat for
    /// the same sample.
    ///
    /// The order is emit-then-advance. A driver that primes with `advance(1)`
    /// before the first block — "advance-then-tick semantics" — puts the two
    /// clocks exactly one `beats_per_sample` apart for the entire render, which
    /// reads as "the samplers are slightly late" and nothing else.
    #[test]
    fn offline_timeline_agrees_with_transport_clock_sample_for_sample() {
        use crate::clock::TransportClock;
        use std::sync::atomic::AtomicBool;

        let sample_rate = 44100.0;
        let tempo = 120.0;
        let start_beat = Beat::new(4.0);

        let mut clock = TransportClock::new(
            crate::state::ClockLinks::bare(Arc::new(AtomicBool::new(false))),
            SampleRate::of(sample_rate),
            Bpm::of(tempo),
        )
        .starting_at(start_beat);

        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat,
            tempo_map: constant(tempo),
            sample_rate: SampleRate::of(sample_rate),
            loop_range: None,
        });

        // Sample 0: both must report the start beat, before either advances.
        let clock_beat = clock.step(1).get();
        assert!(
            (clock_beat - timeline.beat().get()).abs() < 1e-9,
            "first sample disagrees: clock={clock_beat} timeline={}",
            timeline.beat().get()
        );

        // And they must stay in step across a block boundary. The driver
        // steps the clock frame by frame, then advances the timeline by the
        // block size.
        let block = 512;
        let mut clock_beat = clock_beat;
        for _ in 1..block {
            clock_beat = clock.step(1).get();
        }
        timeline.advance(Samples::new(block));

        let expected_lag =
            tutti_music::beats_per_sample(Bpm::of(tempo), SampleRate::of(sample_rate));
        // After the block the timeline sits one sample ahead of the last
        // EMITTED sample, because emit-then-advance means sample N-1 carried
        // the beat before the final increment.
        assert!(
            ((timeline.beat().get() - clock_beat) - expected_lag.get()).abs() < 1e-9,
            "drifted across the block: clock={clock_beat} timeline={} \
             (expected exactly one beats_per_sample apart)",
            timeline.beat().get()
        );
    }

    /// At 90 BPM / 48 kHz, 1 500 blocks of 64 frames
    /// is beat 3 and 500 is beat 1, to the bit. An accumulated playhead
    /// (`beat += beats_per_sample × 64` per block) would read `2.999999999999891`
    /// and `1.0000000000000007`.
    #[test]
    fn a_block_on_a_beat_is_that_beat_exactly() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(0.0),
            tempo_map: constant(90.0),
            sample_rate: SampleRate::SR_48K,
            loop_range: None,
        });
        for _ in 0..500 {
            timeline.advance(Samples::new(64));
        }
        assert_eq!(timeline.beat(), Beat::new(1.0));
        for _ in 500..1_500 {
            timeline.advance(Samples::new(64));
        }
        assert_eq!(timeline.beat(), Beat::new(3.0));
    }

    /// After N blocks of arbitrary lengths, at arbitrary tempos and rates,
    /// both clocks of a render — this timeline, advanced a block at a time,
    /// and a `TransportClock`, stepped 64 frames a block — stand on the closed form `start + frames × tempo / (60 × rate)`, to
    /// the bit. Correctly rounded `*`, `/` and `+` only (no libm), so the
    /// comparison is portable.
    #[test]
    fn a_long_render_stands_on_the_closed_form() {
        use crate::clock::TransportClock;
        use std::sync::atomic::AtomicBool;

        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20 {
            let tempo = 40.0 + (next() % 20_000) as f64 / 100.0;
            let rate = [44_100.0, 48_000.0, 96_000.0][(next() % 3) as usize];
            let start = (next() % 64) as f64 / 3.0;
            let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
                start_beat: Beat::new(start),
                tempo_map: constant(tempo),
                sample_rate: SampleRate::of(rate),
                loop_range: None,
            });
            let mut clock = TransportClock::new(
                crate::state::ClockLinks::bare(Arc::new(AtomicBool::new(false))),
                SampleRate::of(rate),
                Bpm::of(tempo),
            )
            .starting_at(Beat::new(start));
            let mut frames = 0u64;
            for _ in 0..1_000 {
                let n = 1 + (next() % 2_048) as usize;
                timeline.advance(Samples::new(n));
                frames += n as u64;
            }
            for _ in 0..frames / 64 {
                clock.step(64);
            }
            clock.step((frames % 64) as usize);
            let closed = start + (frames as f64 * tempo) / (60.0 * rate);
            assert_eq!(
                timeline.beat().get().to_bits(),
                closed.to_bits(),
                "the timeline, {tempo} BPM at {rate} Hz, {frames} frames"
            );
            assert_eq!(
                clock.current_beat().get().to_bits(),
                closed.to_bits(),
                "the clock, {tempo} BPM at {rate} Hz, {frames} frames"
            );
        }
    }

    #[test]
    fn test_timeline_seek_to() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(0.0),
            tempo_map: constant(120.0),
            sample_rate: SampleRate::SR_44K1,
            loop_range: None,
        });

        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance(Samples::new((5.0 * samples_per_beat) as usize));
        assert!((timeline.beat().get() - 5.0).abs() < 0.01);

        // Seek back to beat 2
        timeline.seek_to(Beat::new(2.0));
        assert!((timeline.beat().get() - 2.0).abs() < 0.001);
    }

    #[test]
    fn timeline_impl_reports_the_loop_region() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(0.0),
            tempo_map: constant(120.0),
            sample_rate: SampleRate::SR_44K1,
            loop_range: LoopRange::new(Beat::new(0.0), Beat::new(8.0)),
        });

        use crate::Timeline;

        assert_eq!(
            timeline.loop_range(),
            LoopRange::new(Beat::new(0.0), Beat::new(8.0))
        );
        assert_eq!(timeline.tempo_map().constant_tempo(), Some(Bpm::of(120.0)));
        // An offline timeline has nothing to pause it.
        assert!(timeline.is_rolling());
    }

    #[test]
    fn timeline_impl_reports_no_loop_when_unset() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(0.0),
            tempo_map: constant(120.0),
            sample_rate: SampleRate::SR_44K1,
            loop_range: None,
        });

        assert_eq!(timeline.loop_range(), None);
    }

    fn map(points: &[(f64, f64)], shape: Transition) -> Arc<TempoMap> {
        use tutti_tempo::TempoPoint;
        let last = points.len() - 1;
        Arc::new(
            TempoMap::new(
                points
                    .iter()
                    .enumerate()
                    .map(|(i, &(beat, bpm))| TempoPoint {
                        beat: Beat::new(beat),
                        tempo: Bpm::of(bpm),
                        transition: if i == last { Transition::Step } else { shape },
                    }),
            )
            .expect("a map"),
        )
    }

    fn config(start: f64) -> OfflineTimelineConfig {
        OfflineTimelineConfig {
            start_beat: Beat::new(start),
            tempo_map: constant(120.0),
            sample_rate: SampleRate::SR_48K,
            loop_range: None,
        }
    }

    /// A graph rendered at `f64` through the timeline keeps a figure `f32`
    /// rounds away in every frame, and the clock advances by the frames
    /// rendered: 100 frames at 120 BPM and 48 kHz are 100 / 24 000 beats.
    #[test]
    fn a_block_rendered_at_f64_keeps_its_figure_and_advances_the_clock_by_its_frames() {
        use tutti_buffers::{ChannelLayout, MaxBlock};
        use tutti_buffers::{Planes, PlanesMut};
        use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};

        // One part in 2^40 above one: inside `f64`'s significand, past `f32`'s.
        const FINE: f64 = 1.0 + 1.0 / 1_099_511_627_776.0;
        #[derive(Clone)]
        struct Dc;
        impl<F: Float> Node<F> for Dc {
            fn shape(&self) -> Shape {
                Shape::source_unassigned(ChannelLayout::MONO)
            }
            fn prepare(&mut self, _: &Prepare) {}
            fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
                io.output(0).fill(F::from_f64(FINE));
                Status::Modified
            }
            fn reset(&mut self) {}
        }

        let mut g = GraphBuilder::<f64>::with_format(ChannelLayout::EMPTY, ChannelLayout::MONO);
        let dc = g.add(ForkByClone(Dc));
        g.connect_output(dc, 0, 0);
        let (mut editor, mut exec) = g
            .build(Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(128)),
            ))
            .expect("builds at f64");
        let timeline = OfflineTimeline::new(&config(0.0));
        let mut out = vec![0.0f64; 100];
        crate::render_graph(
            &timeline,
            &mut exec,
            Planes::default(),
            PlanesMut::new(&mut [&mut out[..]]).expect("one plane"),
        );
        editor.collect();
        assert!(out.iter().all(|&x| x == FINE));
        assert_eq!(timeline.beat(), Beat::new(100.0 / 24_000.0));
    }

    /// A block of zero frames renders nothing and leaves the clock where it
    /// was: the output keeps what it held and the playhead does not move.
    #[test]
    fn a_zero_frame_block_renders_nothing_and_leaves_the_clock() {
        use tutti_buffers::{ChannelLayout, MaxBlock};
        use tutti_buffers::{Planes, PlanesMut};
        use tutti_graph::{GraphBuilder, Prepare};

        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
        g.set_output(0, tutti_graph::PortSource::Zero);
        let (_editor, mut exec) = g
            .build(Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(128)),
            ))
            .expect("builds");
        let timeline = OfflineTimeline::new(&config(0.0));
        let mut none: [f32; 0] = [];
        crate::render_graph(
            &timeline,
            &mut exec,
            Planes::default(),
            PlanesMut::new(&mut [&mut none[..]]).expect("one empty plane"),
        );
        assert_eq!(timeline.beat(), Beat::new(0.0));
        assert_eq!(exec.frame().get(), 0);
    }

    /// A ramp is followed: from beat 0 of 97 to 120 BPM over 4 beats (tempo
    /// linear in time, so `T = 2 x 4 / (97 + 120) x 60` s) the first cell's
    /// tempo is the chord of the curve over its 128 frames, written out here
    /// from the closed form `beats(s) = r0 s + a s^2 / 2`, `a = (r1 - r0) / T`.
    #[test]
    fn a_ramp_is_followed_cell_by_cell() {
        let ramp = OfflineTimeline::new(&OfflineTimelineConfig {
            tempo_map: map(
                &[(0.0, 97.0), (4.0, 120.0)],
                Transition::RampTime { end: None },
            ),
            ..config(0.0)
        });
        let (r0, r1) = (97.0 / 60.0, 120.0 / 60.0);
        let t = 2.0 * 4.0 / (r0 + r1);
        let a = (r1 - r0) / t;
        let s = 128.0 / 48_000.0;
        let chord = (r0 * s + a * s * s / 2.0) * 60.0 * 48_000.0 / 128.0;
        let first = ramp.graph_block().0.tempo.get();
        assert!((first - chord).abs() < 1e-9, "{first} vs {chord}");
        assert!(first > 97.0);
        // 128 frames on, the next cell's chord is higher.
        ramp.advance(Samples::new(128));
        assert!(ramp.graph_block().0.tempo.get() > first);
    }

    /// The timeline reports the map it was given, not a tempo of its own.
    #[test]
    fn the_timeline_reports_the_map_it_was_given() {
        use crate::Timeline;
        let given = map(
            &[(0.0, 97.0), (4.0, 120.0)],
            Transition::RampTime { end: None },
        );
        let t = OfflineTimeline::new(&OfflineTimelineConfig {
            tempo_map: Arc::clone(&given),
            ..config(0.0)
        });
        assert!(Arc::ptr_eq(&t.tempo_map(), &given));
    }

    /// 41 steps 24 frames apart: `block(512)` holds the first 8 and ends on the
    /// frame of the ninth, which is the first frame at or after its second
    /// (written out: beat 0.001 x 9 at its segments' tempos, 120 + i BPM).
    #[test]
    fn a_block_ends_where_the_ninth_change_would_begin() {
        let points: Vec<(f64, f64)> = (0..41)
            .map(|i| (f64::from(i) * 0.001, 120.0 + f64::from(i)))
            .collect();
        let t = OfflineTimeline::new(&OfflineTimelineConfig {
            tempo_map: map(&points, Transition::Step),
            ..config(0.0)
        });
        let (first, changes, n) = t.block(512);
        assert_eq!(first.tempo, Bpm::of(120.0));
        assert_eq!(changes.len(), tutti_graph::MAX_TRANSPORT_CHANGES);
        let sec: f64 = (0..9).map(|i| 0.001 * 60.0 / (120.0 + f64::from(i))).sum();
        let ninth = tutti_music::first_frame_at_or_after(sec * 48_000.0) as usize;
        assert_eq!(n, ninth);
        assert!(n < 512);
        // The eighth, one change earlier.
        let sec8: f64 = (0..8).map(|i| 0.001 * 60.0 / (120.0 + f64::from(i))).sum();
        assert_eq!(
            changes.as_slice()[7].at.index(),
            tutti_music::first_frame_at_or_after(sec8 * 48_000.0) as usize
        );
    }

    /// A seek on a timeline over a map lands on the exact beat in the step it
    /// is in: to 4.5 of a step to 60 BPM at beat 4, 48 000 frames on is beat
    /// 5.5; back to 3.5 at 120 BPM, 12 000 frames reach beat 4 and the 60 BPM.
    #[test]
    fn a_seek_on_a_step_map_takes_the_tempo_of_its_step() {
        let t = OfflineTimeline::new(&OfflineTimelineConfig {
            tempo_map: map(&[(0.0, 120.0), (4.0, 60.0)], Transition::Step),
            ..config(0.0)
        });
        t.seek_to(Beat::new(4.5));
        assert_eq!(t.graph_block().0.tempo, Bpm::of(60.0));
        t.advance(Samples::new(48_000));
        assert_eq!(t.beat(), Beat::new(5.5));
        t.seek_to(Beat::new(3.5));
        assert_eq!(t.graph_block().0.tempo, Bpm::of(120.0));
        t.advance(Samples::new(11_999));
        assert_eq!(t.graph_block().0.tempo, Bpm::of(120.0));
        t.advance(Samples::new(1));
        assert_eq!(
            (t.beat(), t.graph_block().0.tempo),
            (Beat::new(4.0), Bpm::of(60.0))
        );
    }
}
