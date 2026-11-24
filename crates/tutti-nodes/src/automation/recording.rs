//! Automation *recording* — the capture side, companion to the playback-side
//! [`ControlLaneNode`](crate::automation::ControlLaneNode).
//!
//! [`AutomationRecorder`] is the **write** side of a [`Curve`]: where a curve is
//! `beat -> value`, a recorder is fed `(beat, value)` samples by whoever watches
//! the param and hands back an [`Trajectory`] — itself a `Curve` — when
//! the take ends. Like every other curve in the engine it holds no clock and
//! consults no loop range: the caller supplies an already-resolved, already
//! loop-wrapped [`Beat`].
//!
//! A recorder is also a `Curve` in its own right, reading the take *in progress*
//! (see the [`Curve`] impl). That is what makes latch-hold a plain curve
//! rather than a second, disagreeing accumulator: for the take's duration the
//! host reads the in-flight take where it would read the saved trajectory, and
//! automation replaces the knob as it always does.
//!
//! ## What lives where
//!
//! One recorder is one lane under capture. The *map* from whatever a host calls a
//! target to its recorder is the host's business, not the engine's — a
//! `HashMap<YourTarget, AutomationRecorder>` in whatever the host already owns. The engine
//! has no opinion on how targets are addressed, so it holds no registry and
//! carries no target trait.

use tutti_curve::{Breakpoint, Law, Trajectory};
use tutti_modulation::Curve;
use tutti_units::{Beat, BeatDuration, Unit};

// ───────────────────────────── mode ──────────────────────────────

/// How a [`AutomationRecorder`] captures — the three recording disciplines every DAW
/// spells the same way.
///
/// There is no `Off` and no `Play`: not recording is the absence of a recorder
/// (the host's map simply has no entry), and *playback* is
/// [`Curve::value_at`] on the saved trajectory. A mode with an `Off` and a `Play`
/// arm would give the recorder reads of its own, which this type deliberately
/// has none of — a recorder that also answered reads would be a second
/// accumulator competing with the saved trajectory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordMode {
    /// Capture continuously from the first sample, no touch required.
    Write,
    /// Capture only while the control is held; the take ends on release.
    Touch,
    /// Capture while held, then hold the released value until the take ends.
    Latch,
}

impl RecordMode {
    /// Whether a take begins on [`touch`](AutomationRecorder::touch) rather than on the
    /// first [`record`](AutomationRecorder::record).
    #[inline]
    #[must_use]
    fn starts_on_touch(self) -> bool {
        matches!(self, Self::Touch | Self::Latch)
    }

    /// Whether [`release`](AutomationRecorder::release) ends the take outright (`Touch`)
    /// rather than holding the released value (`Latch`).
    #[inline]
    #[must_use]
    pub fn stops_on_release(self) -> bool {
        matches!(self, Self::Touch)
    }
}

// ──────────────────────────── config ─────────────────────────────

/// Capture policy: how densely points are written, and whether the take is
/// thinned when it ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordingConfig {
    min_point_interval: BeatDuration,
    simplify_tolerance: f64,
    auto_simplify: bool,
    default_curve: Law,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingConfig {
    /// The capture policy a recorder starts with: points at least 0.01 beat apart, the take
    /// thinned on `finish` to a tolerance of 0.01, and linear curves.
    #[must_use]
    pub fn new() -> Self {
        Self {
            min_point_interval: BeatDuration::new(0.01),
            simplify_tolerance: 0.01,
            auto_simplify: true,
            default_curve: Law::Linear,
        }
    }

    /// The minimum beat gap between recorded points. A `record` closer than this to the
    /// previous point is dropped, so a control dragged at frame rate does not write one
    /// point per frame.
    #[must_use]
    pub fn min_point_interval(mut self, gap: BeatDuration) -> Self {
        self.min_point_interval = gap;
        self
    }

    /// The Douglas-Peucker tolerance [`AutomationRecorder::finish`] thins with when
    /// [`auto_simplify`](Self::auto_simplify) is set: a distance in the unit's raw value, not
    /// beats.
    #[must_use]
    pub fn simplify_tolerance(mut self, tolerance: f64) -> Self {
        self.simplify_tolerance = tolerance;
        self
    }

    /// Whether [`AutomationRecorder::finish`] thins the take before handing it back.
    #[must_use]
    pub fn auto_simplify(mut self, on: bool) -> Self {
        self.auto_simplify = on;
        self
    }

    /// The curve type stamped on each recorded point.
    #[must_use]
    pub fn default_curve(mut self, curve: Law) -> Self {
        self.default_curve = curve;
        self
    }
}

// ───────────────────────────── take ──────────────────────────────

/// The take in progress. Absent between takes.
///
/// The two variants are the *whole* difference between `Touch` and `Latch`:
/// both capture while held, and on release `Touch` drops to `None` while `Latch`
/// drops to `Held`. Stating that as a variant is what keeps the mode check out
/// of the read path.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Take<U> {
    /// The control is held. Samples land in the trajectory.
    Capturing { last_beat: Beat, last_value: U },
    /// Released under `Latch` — the value is frozen until the take ends.
    Held { value: U },
}

// ──────────────────────────── recorder ───────────────────────────

/// One automation lane under capture: the write side of a [`Curve`].
///
/// Takes `&mut self` throughout. The previous incarnation held its trajectory in
/// an `Arc<RwLock<_>>` so a concurrent map could hand out `&self` recorders;
/// with the map host-side that lock had no reader to protect, and the borrow
/// checker does the job for free. A recorder has no concurrent reader to
/// share it with.
#[derive(Debug, Clone)]
pub struct AutomationRecorder<U: Unit<Raw = f64>> {
    trajectory: Trajectory<U>,
    mode: RecordMode,
    take: Option<Take<U>>,
    config: RecordingConfig,
}

impl<U: Unit<Raw = f64>> AutomationRecorder<U> {
    /// A recorder over an empty trajectory clamped to `[min, max]`.
    #[must_use]
    pub fn new(mode: RecordMode, min: U, max: U) -> Self {
        Self::with_config(mode, min, max, RecordingConfig::new())
    }

    /// [`new`](Self::new) with an explicit capture policy.
    #[must_use]
    pub fn with_config(mode: RecordMode, min: U, max: U, config: RecordingConfig) -> Self {
        Self {
            trajectory: Trajectory::new().with_range(min, max),
            mode,
            take: None,
            config,
        }
    }

    /// A recorder that overdubs onto an existing trajectory — a second pass over a
    /// lane that already has points.
    pub fn overdub(mode: RecordMode, trajectory: Trajectory<U>) -> Self {
        Self {
            trajectory,
            mode,
            take: None,
            config: RecordingConfig::new(),
        }
    }

    /// The capture discipline currently in force.
    pub fn mode(&self) -> RecordMode {
        self.mode
    }

    /// Whether a take is currently open (capturing, or latch-held).
    #[must_use]
    pub fn is_taking(&self) -> bool {
        self.take.is_some()
    }

    /// The control was grabbed: open a take at `(beat, value)`.
    ///
    /// A no-op in [`Write`](RecordMode::Write), which captures from the first
    /// [`record`](Self::record) without waiting to be touched.
    pub fn touch(&mut self, beat: Beat, value: U) {
        if !self.mode.starts_on_touch() {
            return;
        }
        self.take = Some(Take::Capturing {
            last_beat: beat,
            last_value: value,
        });
        self.write_point(beat, value);
    }

    /// A sample from the watched param. Written only while a take is capturing,
    /// and only if `min_point_interval` beats have passed since the last point.
    pub fn record(&mut self, beat: Beat, value: U) {
        match self.take {
            // Write opens its own take on first sample — nothing to touch.
            None if self.mode == RecordMode::Write => {
                self.take = Some(Take::Capturing {
                    last_beat: beat,
                    last_value: value,
                });
            }
            Some(Take::Capturing { last_beat, .. }) => {
                // Thin at the configured density. Only a *forward* gap counts:
                // a backwards jump is a seek, which should write immediately
                // rather than be swallowed as "too soon".
                let gap = beat - last_beat;
                if gap > BeatDuration::new(0.0) && gap < self.config.min_point_interval {
                    return;
                }
            }
            // No take (Touch/Latch untouched), or latch-held — neither captures.
            _ => return,
        }

        self.write_point(beat, value);
        self.take = Some(Take::Capturing {
            last_beat: beat,
            last_value: value,
        });
    }

    /// The control was let go.
    ///
    /// `Touch` ends the take. `Latch` freezes `value` until [`finish`](Self::finish).
    /// `Write` ignores this — it captures until the take ends.
    pub fn release(&mut self, beat: Beat, value: U) {
        if self.take.is_none() {
            return;
        }
        if self.mode.stops_on_release() {
            self.write_point(beat, value);
            self.take = None;
        } else if self.mode == RecordMode::Latch {
            self.write_point(beat, value);
            self.take = Some(Take::Held { value });
        }
    }

    /// End the take and hand back the trajectory, thinned if
    /// [`auto_simplify`](RecordingConfig::auto_simplify) is set.
    ///
    /// Takes `self`: the take is over, so consuming the recorder is what the
    /// host's map does anyway (remove the entry, keep the trajectory). This is the
    /// only way to get the trajectory by value.
    #[must_use]
    pub fn finish(mut self) -> Trajectory<U> {
        if self.config.auto_simplify {
            self.trajectory.simplify(self.config.simplify_tolerance);
        }
        self.trajectory
    }

    fn write_point(&mut self, beat: Beat, value: U) {
        self.trajectory
            .add_breakpoint(Breakpoint::new(beat, value).law(self.config.default_curve));
    }
}

/// The take **in progress** — not the saved lane.
///
/// This is what makes latch-hold composable: the host reads the live recorder
/// in place of the saved trajectory for the take's duration — no second
/// summation path.
///
/// `None` when no take is open, preserving the empty-vs-zero distinction
/// [`Curve`] requires: between takes the recorder contributes *nothing* and the
/// saved trajectory shows through, rather than pinning the param to `0.0`.
impl<U: tutti_graph::Scalar> Curve for AutomationRecorder<U> {
    fn value_at(&self, beat: Beat) -> Option<f64> {
        let lane = |u: U| u.to_raw();
        match self.take {
            Some(Take::Held { value }) => Some(lane(value)),
            Some(Take::Capturing { .. }) => self.trajectory.value_at(beat).map(lane),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_units::Db;

    fn recorder(mode: RecordMode) -> AutomationRecorder<Db> {
        let mut r = AutomationRecorder::new(mode, Db::new(0.0), Db::new(1.0));
        // Point-count assertions below are about capture, not thinning.
        r.config = r.config.auto_simplify(false);
        r
    }

    #[test]
    fn a_new_policy_writes_points_a_hundredth_of_a_beat_apart_and_thins_linear_takes() {
        let c = RecordingConfig::new();
        assert_eq!(c.min_point_interval, BeatDuration::new(0.01));
        assert_eq!(c.simplify_tolerance, 0.01);
        assert!(c.auto_simplify);
        assert_eq!(c.default_curve, Law::Linear);
    }

    // ── mode ──

    #[test]
    fn write_captures_without_touch() {
        let mut r = recorder(RecordMode::Write);
        r.record(Beat::new(0.0), Db::new(0.1));
        r.record(Beat::new(1.0), Db::new(0.2));
        r.record(Beat::new(2.0), Db::new(0.3));
        assert_eq!(r.trajectory.len(), 3);
    }

    #[test]
    fn touch_and_latch_ignore_record_until_touched() {
        for mode in [RecordMode::Touch, RecordMode::Latch] {
            let mut r = recorder(mode);
            r.record(Beat::new(0.0), Db::new(0.5));
            r.record(Beat::new(1.0), Db::new(0.6));
            assert!(
                r.trajectory.is_empty(),
                "{mode:?} must not capture before touch"
            );
            assert!(!r.is_taking());
        }
    }

    #[test]
    fn touch_on_write_is_a_noop() {
        let mut r = recorder(RecordMode::Write);
        r.touch(Beat::new(0.0), Db::new(0.5));
        assert!(r.trajectory.is_empty());
        assert!(!r.is_taking());
    }

    // ── take lifecycle ──

    #[test]
    fn touch_records_then_release_ends_the_take() {
        let mut r = recorder(RecordMode::Touch);
        r.touch(Beat::new(0.0), Db::new(0.5));
        assert_eq!(r.trajectory.len(), 1);

        r.record(Beat::new(1.0), Db::new(0.6));
        r.record(Beat::new(2.0), Db::new(0.7));
        assert_eq!(r.trajectory.len(), 3);

        r.release(Beat::new(3.0), Db::new(0.8));
        assert_eq!(r.trajectory.len(), 4, "release writes a final point");
        assert!(!r.is_taking(), "Touch ends the take on release");
    }

    #[test]
    fn touch_stops_capturing_after_release() {
        let mut r = recorder(RecordMode::Touch);
        r.touch(Beat::new(0.0), Db::new(0.5));
        r.release(Beat::new(1.0), Db::new(0.6));
        let after_release = r.trajectory.len();

        r.record(Beat::new(2.0), Db::new(0.9));
        assert_eq!(
            r.trajectory.len(),
            after_release,
            "a released Touch take must not resume on further samples"
        );
    }

    #[test]
    fn latch_holds_after_release() {
        let mut r = recorder(RecordMode::Latch);
        r.touch(Beat::new(2.0), Db::new(0.5));
        r.record(Beat::new(3.0), Db::new(0.6));
        r.release(Beat::new(4.0), Db::new(0.7));

        assert!(r.is_taking(), "Latch keeps the take open");
        assert_eq!(
            r.value_at(Beat::new(5.0)),
            Some(0.7),
            "held value reads at any later beat"
        );
        assert_eq!(
            r.value_at(Beat::new(100.0)),
            Some(0.7),
            "the hold is beat-independent"
        );
    }

    #[test]
    fn latch_does_not_capture_while_held() {
        let mut r = recorder(RecordMode::Latch);
        r.touch(Beat::new(0.0), Db::new(0.5));
        r.release(Beat::new(1.0), Db::new(0.7));
        let held = r.trajectory.len();

        r.record(Beat::new(2.0), Db::new(0.9));
        assert_eq!(
            r.trajectory.len(),
            held,
            "a held latch take freezes; samples do not land"
        );
        assert_eq!(
            r.value_at(Beat::new(2.0)),
            Some(0.7),
            "still reading the hold"
        );
    }

    #[test]
    fn release_without_a_take_is_a_noop() {
        let mut r = recorder(RecordMode::Touch);
        r.release(Beat::new(1.0), Db::new(0.5));
        assert!(r.trajectory.is_empty());
        assert!(!r.is_taking());
    }

    // ── curve impl ──

    #[test]
    fn no_take_contributes_nothing() {
        // The empty-vs-zero distinction: between takes the saved trajectory must
        // show through, so the recorder reads None rather than 0.0.
        let mut r = recorder(RecordMode::Touch);
        assert_eq!(r.value_at(Beat::new(0.0)), None);

        r.touch(Beat::new(0.0), Db::new(0.5));
        r.release(Beat::new(1.0), Db::new(0.6));
        assert_eq!(
            r.value_at(Beat::new(2.0)),
            None,
            "Touch releases to nothing"
        );
    }

    #[test]
    fn capturing_reads_the_trajectory_being_written() {
        let mut r = recorder(RecordMode::Touch);
        r.touch(Beat::new(0.0), Db::new(0.0));
        r.record(Beat::new(4.0), Db::new(1.0));
        // Mid-take the recorder reads its own in-progress curve, interpolated.
        let mid = r
            .value_at(Beat::new(2.0))
            .expect("capturing take has a value");
        assert!((mid - 0.5).abs() < 0.01, "expected ~0.5, got {mid}");
    }

    // ── thinning ──

    #[test]
    fn min_interval_drops_dense_samples() {
        let mut r = AutomationRecorder::with_config(
            RecordMode::Write,
            Db::new(0.0),
            Db::new(1.0),
            RecordingConfig::new()
                .min_point_interval(BeatDuration::new(0.5))
                .auto_simplify(false),
        );

        r.record(Beat::new(0.0), Db::new(0.5)); // opens the take, writes
        r.record(Beat::new(0.1), Db::new(0.6)); // +0.1 — too soon
        r.record(Beat::new(0.4), Db::new(0.7)); // +0.4 from 0.0 — still too soon
        r.record(Beat::new(0.5), Db::new(0.8)); // +0.5 — writes
        assert_eq!(r.trajectory.len(), 2);
    }

    #[test]
    fn a_backwards_jump_always_writes() {
        // A seek during a take is not "too soon" — the gap is negative, so the
        // density check must not swallow it.
        let mut r = AutomationRecorder::with_config(
            RecordMode::Write,
            Db::new(0.0),
            Db::new(1.0),
            RecordingConfig::new()
                .min_point_interval(BeatDuration::new(0.5))
                .auto_simplify(false),
        );
        r.record(Beat::new(4.0), Db::new(0.5));
        r.record(Beat::new(0.0), Db::new(0.9));
        assert_eq!(r.trajectory.len(), 2, "the seek-back point must land");
    }

    #[test]
    fn finish_simplifies_when_configured() {
        let mut r = AutomationRecorder::new(RecordMode::Write, Db::new(0.0), Db::new(1.0));
        r.config = r.config.auto_simplify(true);
        // A dead-straight ramp: every interior point is redundant.
        r.config = r.config.min_point_interval(BeatDuration::new(0.0));
        for i in 0..=10 {
            r.record(Beat::new(i as f64), Db::new(f64::from(i) / 10.0));
        }
        let dense = r.trajectory.len();
        let env = r.finish();
        assert!(
            env.len() < dense,
            "collinear points should thin: {dense} -> {}",
            env.len()
        );
    }

    #[test]
    fn finish_preserves_points_when_not_simplifying() {
        let mut r = recorder(RecordMode::Write);
        r.record(Beat::new(0.0), Db::new(0.1));
        r.record(Beat::new(1.0), Db::new(0.9));
        r.record(Beat::new(2.0), Db::new(0.2));
        let env = r.finish();
        assert_eq!(env.len(), 3);
    }

    // ── overdub ──

    #[test]
    fn overdub_starts_from_an_existing_trajectory() {
        let mut env = Trajectory::new().with_range(Db::new(0.0), Db::new(1.0));
        env.add_breakpoint(Breakpoint::new(Beat::new(0.0), Db::new(0.2)));
        env.add_breakpoint(Breakpoint::new(Beat::new(8.0), Db::new(0.4)));

        let mut r = AutomationRecorder::overdub(RecordMode::Touch, env);
        assert_eq!(r.trajectory.len(), 2);
        assert!(!r.is_taking());

        r.touch(Beat::new(4.0), Db::new(0.9));
        assert_eq!(
            r.trajectory.len(),
            3,
            "the new point joins the existing ones"
        );
    }
}
