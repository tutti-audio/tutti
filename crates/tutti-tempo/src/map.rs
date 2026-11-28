use crate::error::TempoError;
use crate::point::{TempoPoint, Transition};
use crate::segment::Seg;
use alloc::vec;
use alloc::vec::Vec;
use tutti_music::first_frame_at_or_after;
use tutti_units::{Beat, BeatDuration, Bpm, Frame, SampleRate, Seconds};

/// An immutable tempo map.
///
/// Built on the control side (`new` and `with_point` allocate); every query
/// borrows it and allocates nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct TempoMap {
    pts: Vec<TempoPoint>,
    segs: Vec<Seg>,
}

/// The tempo a segment out of a point with `transition` reaches at the beat of
/// `next`: the ramp's own end, else `next`'s tempo.
fn reached(transition: Transition, next: &TempoPoint) -> Bpm {
    match transition {
        Transition::RampTime { end } | Transition::RampBeats { end } => end.unwrap_or(next.tempo),
        Transition::Step => next.tempo,
    }
}

/// A lookup hint, owned by a [`TempoView`](crate::TempoView). Audio asks for
/// nearly monotone positions, so the segment found last time is nearly always
/// right; a wrong hint costs one binary search and never changes a result.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cursor(usize);

impl TempoMap {
    /// One tempo from beat 0 on.
    ///
    /// # Errors
    ///
    /// [`TempoError::Unrepresentable`] when `tempo` is so small that a beat's
    /// rate underflows.
    pub fn constant(tempo: Bpm) -> Result<Self, TempoError> {
        Self::new([TempoPoint::step(Beat::new(0.0), tempo)])
    }

    /// A [`TempoMapBuilder`] that starts at `first` from beat 0, for a map
    /// written point by point with no way to end on a ramp.
    pub fn builder(first: Bpm) -> TempoMapBuilder {
        TempoMapBuilder::new(first)
    }

    /// Build from points sorted by beat. Allocates; control thread only.
    ///
    /// # Errors
    ///
    /// A [`TempoError`] naming what is wrong: no points, a first point off
    /// beat 0, beats that are not finite and strictly increasing, a last point that ramps, or a segment
    /// whose duration an `f64` cannot hold.
    pub fn new(points: impl IntoIterator<Item = TempoPoint>) -> Result<Self, TempoError> {
        let mut pts: Vec<TempoPoint> = points.into_iter().collect();
        let first = pts.first().ok_or(TempoError::Empty)?;
        if first.beat.get() != 0.0 {
            return Err(TempoError::FirstNotAtZero);
        }
        for (i, p) in pts.iter().enumerate() {
            if i > 0 && !(p.beat.get().is_finite() && p.beat > pts[i - 1].beat) {
                return Err(TempoError::BadOrder(i));
            }
        }
        if pts.last().is_some_and(|p| p.transition != Transition::Step) {
            return Err(TempoError::RampWithoutEnd);
        }
        // One representation per map: a ramp's end equal to the next tempo is
        // no end. The last point is a step, so no ramp is left without a next.
        for i in 1..pts.len() {
            let next = pts[i].tempo;
            if let Transition::RampTime { end } | Transition::RampBeats { end } =
                &mut pts[i - 1].transition
                && *end == Some(next)
            {
                *end = None;
            }
        }
        // Anchors: a Neumaier-compensated running sum of segment durations, so
        // a map of 10^5 points keeps the error of one addition, not of 10^5.
        let mut segs = Vec::with_capacity(pts.len());
        let (mut sum, mut comp) = (0.0f64, 0.0f64);
        for (i, p) in pts.iter().enumerate() {
            let (r1, db) = match pts.get(i + 1) {
                Some(n) => (reached(p.transition, n).get() / 60.0, n.beat - p.beat),
                None => (p.tempo.get() / 60.0, BeatDuration::new(f64::INFINITY)),
            };
            let s = Seg::new(
                p.beat,
                Seconds::new(sum + comp),
                p.tempo,
                r1,
                p.transition,
                db,
            );
            if i + 1 < pts.len() {
                if !s.is_sound(db) {
                    return Err(TempoError::Unrepresentable(i));
                }
                // The compensated sum's own arithmetic is on the raw seconds.
                let dsec = s.dsec.get();
                let t = sum + dsec;
                comp += if sum.abs() >= dsec.abs() {
                    (sum - t) + dsec
                } else {
                    (dsec - t) + sum
                };
                sum = t;
                if !(sum + comp).is_finite() {
                    return Err(TempoError::Unrepresentable(i));
                }
            } else if !(s.r0.is_finite() && s.r0 > 0.0) {
                return Err(TempoError::Unrepresentable(i));
            }
            segs.push(s);
        }
        Ok(Self { pts, segs })
    }

    /// A new map with `point` added, or replacing the point at its beat. The
    /// old map is untouched: this is what a control thread publishes.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), for the edited list.
    pub fn with_point(&self, point: TempoPoint) -> Result<Self, TempoError> {
        // `-0.0 + 0.0` is `+0.0`: `total_cmp` would otherwise order a negative
        // zero before the point at beat 0 and insert instead of replace.
        let point = TempoPoint {
            beat: Beat::new(point.beat.get() + 0.0),
            ..point
        };
        let mut pts = self.pts.clone();
        match pts.binary_search_by(|p| p.beat.get().total_cmp(&point.beat.get())) {
            Ok(i) => pts[i] = point,
            Err(i) => pts.insert(i, point),
        }
        Self::new(pts)
    }

    /// This map with `tempo` taking over at `beat`: the map up to `beat` as it
    /// stands, then a step to `tempo`, and nothing after. `Ok(None)` when that
    /// changes nothing (the map already holds `tempo` from `beat` on, with no
    /// point after it).
    ///
    /// A ramp `beat` lands in is cut there: the ramp is kept up to `beat` (it
    /// reaches the tempo this map has at `beat`, so every beat before `beat`
    /// keeps its time), and the tempo steps at that same beat
    /// ([`TempoMapBuilder::then_step`]): the tempo just below `beat` is the
    /// ramp's, at `beat` it is `tempo`. A
    /// `beat` before 0 is treated as the origin, which is what a pre-roll beat
    /// wants. The old map is untouched.
    ///
    /// # Errors
    ///
    /// A [`TempoError`] when the result cannot be held (a tempo so small that a
    /// segment's duration overflows).
    pub fn stepped_to(&self, beat: Beat, tempo: Bpm) -> Result<Option<Self>, TempoError> {
        let beat = beat.max(Beat::new(0.0));
        let points = self.points();
        if points
            .last()
            .is_some_and(|last| last.beat <= beat && last.tempo == tempo)
        {
            return Ok(None);
        }
        let kept = points.partition_point(|p| p.beat < beat);
        let Some(first) = points[..kept].first() else {
            return Self::constant(tempo).map(Some);
        };
        let mut builder = Self::builder(first.tempo);
        for pair in points[..kept].windows(2) {
            builder = builder.transition_to(pair[0].transition, pair[1].beat, pair[1].tempo);
        }
        let last = &points[kept - 1];
        // A ramp is cut at `beat`: it reaches what the map has there (its own
        // `end` when a point sits on `beat`), and the tempo steps there.
        let at_point = points.get(kept).filter(|p| p.beat == beat);
        builder = match last.transition {
            Transition::Step => builder.step_to(beat, tempo),
            Transition::RampTime { end } => builder
                .transition_to(
                    Transition::RampTime { end: None },
                    beat,
                    self.cut_tempo(end, at_point, beat),
                )
                .then_step(tempo),
            Transition::RampBeats { end } => builder
                .transition_to(
                    Transition::RampBeats { end: None },
                    beat,
                    self.cut_tempo(end, at_point, beat),
                )
                .then_step(tempo),
        };
        builder.build().map(Some)
    }

    /// The tempo a ramp cut at `beat` reaches: its own `end` where a point sits
    /// on `beat`, else the map's tempo there.
    fn cut_tempo(&self, end: Option<Bpm>, at_point: Option<&TempoPoint>, beat: Beat) -> Bpm {
        match (end, at_point) {
            (Some(end), Some(_)) => end,
            _ => self.tempo_at(beat),
        }
    }

    /// The points, as built.
    #[must_use]
    pub fn points(&self) -> &[TempoPoint] {
        &self.pts
    }

    /// The beat of the first point after `beat`, or `None` when no point
    /// follows it.
    #[must_use]
    pub fn point_after(&self, beat: Beat) -> Option<Beat> {
        self.pts
            .get(self.pts.partition_point(|p| p.beat.get() <= beat.get()))
            .map(|p| p.beat)
    }

    /// This map continued by a step to each of `steps`, in order: the points
    /// as built, then one `step` per `(beat, tempo)`, built once. The steps'
    /// beats follow the last point's and strictly increase.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), for the extended list.
    pub fn with_steps(
        &self,
        steps: impl IntoIterator<Item = (Beat, Bpm)>,
    ) -> Result<Self, TempoError> {
        Self::new(
            self.pts.iter().copied().chain(
                steps
                    .into_iter()
                    .map(|(beat, tempo)| TempoPoint::step(beat, tempo)),
            ),
        )
    }

    /// The tempo, when the whole map is one point (what
    /// [`constant`](Self::constant) builds): `Some` of that point's tempo to
    /// the bit, `None` for a map that changes. A reader that can follow only
    /// one tempo asks this and refuses the rest.
    #[must_use]
    pub fn constant_tempo(&self) -> Option<Bpm> {
        match self.pts.as_slice() {
            [only] => Some(only.tempo),
            _ => None,
        }
    }

    /// The segment holding `x` by `key` (`beat0` or `sec0`), starting from the
    /// hint. Segment `i` holds `key(i) <= x < key(i + 1)`; below the first it is
    /// the first.
    #[inline]
    fn locate<T: PartialOrd + Copy>(
        &self,
        key: impl Fn(&Seg) -> T,
        x: T,
        hint: &mut Cursor,
    ) -> usize {
        let holds =
            |i: usize| key(&self.segs[i]) <= x && self.segs.get(i + 1).is_none_or(|n| x < key(n));
        let h = hint.0.min(self.segs.len() - 1);
        let i = if holds(h) {
            h
        } else if h + 1 < self.segs.len() && holds(h + 1) {
            h + 1
        } else {
            self.segs.partition_point(|s| key(s) <= x).saturating_sub(1)
        };
        hint.0 = i;
        i
    }

    fn by_beat(&self, beat: Beat, hint: &mut Cursor) -> &Seg {
        &self.segs[self.locate(|s| s.beat0, beat, hint)]
    }

    fn by_sec(&self, sec: Seconds, hint: &mut Cursor) -> &Seg {
        &self.segs[self.locate(|s| s.sec0, sec, hint)]
    }

    /// The segment that owns `frame`, with the frame where it starts and the
    /// frame where the next one does. A point's frame is the first frame at or
    /// after its time ([`first_frame_at_or_after`]); that frame and the ones
    /// after it belong to the point.
    pub(crate) fn by_frame(
        &self,
        frame: Frame,
        rate: SampleRate,
        hint: &mut Cursor,
    ) -> (&Seg, u64, Option<u64>) {
        let cut = |s: &Seg| first_frame_at_or_after(s.sec0.get() * rate.get()).max(0) as u64;
        let i = self.locate(cut, frame.get(), hint);
        (
            &self.segs[i],
            cut(&self.segs[i]),
            self.segs.get(i + 1).map(cut),
        )
    }

    /// Time from beat 0 to `beat` (negative before beat 0, at the first
    /// tempo).
    ///
    /// One query from a cold start. A caller that asks many nearby positions
    /// takes a [`view`](Self::view) instead.
    #[must_use]
    pub fn seconds_at(&self, beat: Beat) -> Seconds {
        self.seconds_at_from(beat, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn seconds_at_from(&self, beat: Beat, hint: &mut Cursor) -> Seconds {
        let s = self.by_beat(beat, hint);
        s.sec0 + s.sec_in(beat - s.beat0)
    }

    /// The beat `seconds` from beat 0.
    #[must_use]
    pub fn beat_at_seconds(&self, seconds: Seconds) -> Beat {
        self.beat_at_seconds_from(seconds, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn beat_at_seconds_from(&self, seconds: Seconds, hint: &mut Cursor) -> Beat {
        let s = self.by_sec(seconds, hint);
        s.beat0 + s.beat_in(seconds - s.sec0)
    }

    /// The tempo at `beat`. A point's own beat belongs to that point (the map is
    /// right-continuous): a step reads its new tempo there, a ramp the tempo it
    /// was given, and a point that ends a ramp at an `end` reads the tempo it
    /// steps to.
    #[must_use]
    pub fn tempo_at(&self, beat: Beat) -> Bpm {
        self.tempo_at_from(beat, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn tempo_at_from(&self, beat: Beat, hint: &mut Cursor) -> Bpm {
        let s = self.by_beat(beat, hint);
        s.bpm_at_beat(beat - s.beat0)
    }

    /// The tempo `seconds` from beat 0.
    #[must_use]
    pub fn tempo_at_seconds(&self, seconds: Seconds) -> Bpm {
        self.tempo_at_seconds_from(seconds, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn tempo_at_seconds_from(&self, seconds: Seconds, hint: &mut Cursor) -> Bpm {
        let s = self.by_sec(seconds, hint);
        s.bpm_at_sec(seconds - s.sec0)
    }

    #[inline]
    pub(crate) fn tempo_slope_from(&self, from: Seconds, to: Seconds, hint: &mut Cursor) -> f64 {
        let s = self.by_sec(from, hint);
        let ds0 = from - s.sec0;
        let ds1 = (to - s.sec0).min(s.dsec);
        let span = (ds1 - ds0).get();
        // `is_nan` too: a NaN span is no span.
        if span.is_nan() || span <= 0.0 {
            return 0.0;
        }
        (s.bpm_at_sec(ds1).get() - s.bpm_at_sec(ds0).get()) / span
    }

    #[inline]
    pub(crate) fn frame_at_or_after_from(
        &self,
        beat: Beat,
        rate: SampleRate,
        hint: &mut Cursor,
    ) -> Frame {
        let frames = self.seconds_at_from(beat, hint).get() * rate.get();
        Frame::new(first_frame_at_or_after(frames).max(0) as u64)
    }

    #[inline]
    pub(crate) fn next_change_from(
        &self,
        frame: Frame,
        rate: SampleRate,
        hint: &mut Cursor,
    ) -> Option<Frame> {
        let (seg, start, next) = self.by_frame(frame, rate, hint);
        // Only the first segment starts at beat 0, and that is not a change.
        if start == frame.get() && seg.beat0 != Beat::new(0.0) {
            return Some(frame);
        }
        next.map(Frame::new)
    }
}

/// A tempo map written left to right, where a trailing ramp cannot be written.
///
/// It starts with a tempo at beat 0 ([`TempoMap::builder`]). Each call after it
/// names the next point and how the tempo gets *to* it from the last one: the
/// point it adds is always a step, so the map's last point never ramps (the
/// case [`TempoError::RampWithoutEnd`] refuses in a list of points).
///
/// ```
/// use tutti_tempo::{TempoMap, Transition};
/// use tutti_units::{Beat, Bpm};
///
/// let map = TempoMap::builder(Bpm::of(120.0))
///     .ramp_time_to(Beat::new(4.0), Bpm::of(180.0))
///     .step_to(Beat::new(8.0), Bpm::of(90.0))
///     .build()?;
/// assert_eq!(map.points()[0].transition, Transition::RampTime { end: None });
/// assert_eq!(map.points().last().unwrap().transition, Transition::Step);
/// # Ok::<(), tutti_tempo::TempoError>(())
/// ```
///
/// A ramp with no point to ramp to has no spelling:
///
/// ```compile_fail,E0599
/// use tutti_tempo::TempoMap;
/// use tutti_units::{Beat, Bpm};
///
/// let _ = TempoMap::builder(Bpm::of(120.0)).ramp_time(Beat::new(4.0));
/// ```
#[derive(Clone, Debug)]
#[must_use]
pub struct TempoMapBuilder {
    points: Vec<TempoPoint>,
}

impl TempoMapBuilder {
    pub(crate) fn new(first: Bpm) -> Self {
        Self {
            points: vec![TempoPoint::step(Beat::new(0.0), first)],
        }
    }

    /// Reach `tempo` at `beat` by `transition`: [`step_to`](Self::step_to),
    /// [`ramp_time_to`](Self::ramp_time_to) or
    /// [`ramp_beats_to`](Self::ramp_beats_to), chosen by a value.
    pub fn transition_to(mut self, transition: Transition, beat: Beat, tempo: Bpm) -> Self {
        if let Some(last) = self.points.last_mut() {
            last.transition = transition;
        }
        self.points.push(TempoPoint::step(beat, tempo));
        self
    }

    /// Hold the last tempo until `beat`, then take `tempo` there.
    pub fn step_to(self, beat: Beat, tempo: Bpm) -> Self {
        self.transition_to(Transition::Step, beat, tempo)
    }

    /// Move from the last tempo to `tempo`, reaching it at `beat`, with tempo
    /// linear in time ([`Transition::RampTime`]).
    pub fn ramp_time_to(self, beat: Beat, tempo: Bpm) -> Self {
        self.transition_to(Transition::RampTime { end: None }, beat, tempo)
    }

    /// Move from the last tempo to `tempo`, reaching it at `beat`, with tempo
    /// linear in beats ([`Transition::RampBeats`]).
    pub fn ramp_beats_to(self, beat: Beat, tempo: Bpm) -> Self {
        self.transition_to(Transition::RampBeats { end: None }, beat, tempo)
    }

    /// The last point departs with `tempo` at its own beat. When the ramp
    /// before it has no `end`, that ramp's `end` becomes the last point's
    /// current tempo (the arrival), so the ramp ends and the tempo steps at one
    /// beat with no plateau between. When the ramp already has an `end`, only
    /// the departure is replaced, so a second call keeps the arrival. After a
    /// step (or on the first point) there is no ramp, and it replaces the last
    /// point's tempo. Every beat before the last point keeps its time.
    ///
    /// ```
    /// use tutti_tempo::TempoMap;
    /// use tutti_units::{Beat, Bpm, Seconds};
    ///
    /// // 120 to 180 BPM over four beats lasts 1.6 s; the tempo is 90 from there.
    /// let map = TempoMap::builder(Bpm::of(120.0))
    ///     .ramp_time_to(Beat::new(4.0), Bpm::of(180.0))
    ///     .then_step(Bpm::of(90.0))
    ///     .build()?;
    /// assert_eq!(map.tempo_at(Beat::new(4.0)), Bpm::of(90.0));
    /// assert!((map.seconds_at(Beat::new(4.0)) - Seconds::new(1.6)).get().abs() < 1e-12);
    /// # Ok::<(), tutti_tempo::TempoError>(())
    /// ```
    pub fn then_step(mut self, tempo: Bpm) -> Self {
        if let Some(last) = self.points.pop() {
            if let Some(Transition::RampTime { end } | Transition::RampBeats { end }) =
                self.points.last_mut().map(|p| &mut p.transition)
            {
                end.get_or_insert(last.tempo);
            }
            self.points.push(TempoPoint { tempo, ..last });
        }
        self
    }

    /// The map.
    ///
    /// # Errors
    ///
    /// As [`TempoMap::new`], for what a builder can still get wrong: a beat
    /// that is not finite or not after the last point's
    /// ([`TempoError::BadOrder`]), or a segment an `f64` cannot hold.
    pub fn build(self) -> Result<TempoMap, TempoError> {
        TempoMap::new(self.points)
    }
}
