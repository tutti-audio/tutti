//! The base of the tempo and the meter: the map the app writes, which a live
//! source's steps lie over.
//!
//! A [`Base`] holds the app's map and the live steps a follower placed
//! (`Steps`). The map the audio thread reads, the effective value, is the base
//! up to the first step and the steps after it. With no step it is the base. A
//! write to the base and a step both change the effective map, which the
//! writer publishes. Control side only.

use std::sync::Arc;

use tutti_music::MeterMap;
use tutti_rt::Mutex;
use tutti_tempo::TempoMap;
use tutti_timeline::TransportSettings;
use tutti_units::{Beat, Bpm};

use super::error::SetRefusal;
use super::slot::{Held, Slot, Stage};

/// A map a live source steps: one change placed at a beat.
pub(crate) trait Steps: Send + Sync + 'static + Sized {
    /// One change a live source placed: where it was asked for, and what it
    /// sets.
    type Step: Copy + Send + Sync + 'static;

    /// The beat `step` was asked for at.
    #[cfg(feature = "midi")]
    fn beat(step: &Self::Step) -> Beat;

    /// Whether `a` and `b` set the same value, whatever their beats.
    #[cfg(feature = "midi")]
    fn same_value(a: &Self::Step, b: &Self::Step) -> bool;

    /// This map with `step` laid over it: the map before the step as it
    /// stands, the step, and nothing after. `None` when that changes nothing.
    fn stepped(&self, step: Self::Step) -> Option<Self>;

    /// `base` with `steps` laid over it, in order: each step truncates what
    /// follows it. The steps are ordered by beat.
    fn laid(base: &Arc<Self>, steps: &[Self::Step]) -> Arc<Self> {
        let mut map = Arc::clone(base);
        for &step in steps {
            if let Some(next) = map.stepped(step) {
                map = Arc::new(next);
            }
        }
        map
    }
}

impl Steps for TempoMap {
    /// The beat a tempo is placed at, and the tempo.
    type Step = (Beat, Bpm);

    #[cfg(feature = "midi")]
    fn beat((beat, _): &Self::Step) -> Beat {
        *beat
    }

    #[cfg(feature = "midi")]
    fn same_value((_, a): &Self::Step, (_, b): &Self::Step) -> bool {
        a == b
    }

    /// One pass: the first step cuts the base once, and each later step is a
    /// point after the last (the steps' beats strictly increase), so the map is
    /// built once over `base`'s points and the steps, not copied per step.
    fn laid(base: &Arc<Self>, steps: &[Self::Step]) -> Arc<Self> {
        let Some((&first, rest)) = steps.split_first() else {
            return Arc::clone(base);
        };
        let cut = base
            .stepped(first)
            .map_or_else(|| Arc::clone(base), Arc::new);
        // The builder refuses only a tempo whose segment duration overflows an
        // `f64`; a followed `Bpm` is far above that, so a refusal keeps the cut.
        cut.with_steps(rest.iter().copied()).map_or(cut, Arc::new)
    }

    fn stepped(&self, (beat, tempo): (Beat, Bpm)) -> Option<Self> {
        // The builder refuses only a tempo whose segment duration overflows an
        // `f64` (below ~1e-300 bpm); a followed `Bpm` is at least 1.397 bpm
        // (`FlexTempo`'s field at `u32::MAX`), so a refusal changes nothing.
        self.stepped_to(beat, tempo).ok().flatten()
    }
}

impl Steps for MeterMap {
    /// The beat a signature is asked for: it lands on the next bar line at or
    /// after it (a meter change always begins a bar).
    type Step = (Beat, tutti_music::TimeSignature);

    #[cfg(feature = "midi")]
    fn beat((beat, _): &Self::Step) -> Beat {
        *beat
    }

    #[cfg(feature = "midi")]
    fn same_value((_, a): &Self::Step, (_, b): &Self::Step) -> bool {
        a == b
    }

    fn stepped(&self, (beat, signature): Self::Step) -> Option<Self> {
        self.with_change_at(self.next_bar_line(beat), signature)
    }
}

struct State<M: Steps> {
    base: Arc<M>,
    steps: Vec<M::Step>,
    /// Whether a write is not yet published.
    staged: bool,
}

/// The app's map and the live steps over it, shared by the value's slot and the
/// follower.
pub(crate) struct Base<M: Steps>(Arc<Mutex<State<M>>>);

impl<M: Steps> Clone for Base<M> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<M: Steps> std::fmt::Debug for Base<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Base").finish_non_exhaustive()
    }
}

impl<M: Steps> Base<M> {
    /// A base `map`, with no step over it.
    pub(crate) fn new(map: Arc<M>) -> Self {
        Self(Arc::new(Mutex::new(State {
            base: map,
            steps: Vec::new(),
            staged: false,
        })))
    }

    /// The app's map.
    pub(crate) fn get(&self) -> Arc<M> {
        Arc::clone(&self.0.lock().base)
    }

    /// Writes the app's map, to be published by the next
    /// [`take_staged`](Self::take_staged).
    pub(crate) fn set(&self, map: Arc<M>) {
        let mut state = self.0.lock();
        state.base = map;
        state.staged = true;
    }

    /// The effective map a write staged and nothing published yet, once.
    fn take_staged(&self) -> Option<Arc<M>> {
        let mut state = self.0.lock();
        std::mem::take(&mut state.staged).then(|| Self::laid(&state))
    }

    /// The base up to the first step and the steps after it.
    fn laid(state: &State<M>) -> Arc<M> {
        M::laid(&state.base, &state.steps)
    }

    /// Records `step`. The steps are a stack ordered by beat: `step` drops
    /// every recorded step at or after its beat, which it truncates, and is
    /// kept only when it sets another value than the step before it, since a
    /// step that repeats the value in force changes no base. A followed source
    /// therefore leaves one step per change of value.
    #[cfg(feature = "midi")]
    pub(crate) fn push(&self, step: M::Step) {
        let mut state = self.0.lock();
        let at = M::beat(&step);
        while state.steps.last().is_some_and(|last| M::beat(last) >= at) {
            state.steps.pop();
        }
        if !state
            .steps
            .last()
            .is_some_and(|last| M::same_value(last, &step))
        {
            state.steps.push(step);
        }
    }

    /// Drops every step: the app's map as it stands now, to publish, when
    /// there were steps. A write staged and not yet published stays staged.
    #[cfg(feature = "midi")]
    pub(crate) fn release(&self) -> Option<Arc<M>> {
        let mut state = self.0.lock();
        if state.steps.is_empty() {
            return None;
        }
        state.steps.clear();
        state.staged = false;
        Some(Arc::clone(&state.base))
    }
}

/// A value held as a [`Base`]: a write lands in the base at once and is
/// published, with the live steps over it, by the next update; a read answers
/// the base, and [`Slot::effective`] the map published.
pub(crate) struct BaseSlot<M: Steps, C: Held<Value = Arc<M>>> {
    cell: C,
    base: Base<M>,
}

impl<M: Steps, C: Held<Value = Arc<M>>> BaseSlot<M, C> {
    /// A slot over `base`, published through `cell`.
    pub(crate) fn new(cell: C, base: Base<M>) -> Arc<Self> {
        Arc::new(Self { cell, base })
    }
}

impl<M: Steps, C: Held<Value = Arc<M>>> Slot<Arc<M>> for BaseSlot<M, C> {
    fn load(&self) -> Arc<M> {
        self.base.get()
    }

    fn effective(&self) -> Arc<M> {
        self.cell.current()
    }

    fn check(&self, _: &Arc<M>) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &Arc<M>) {
        self.base.set(Arc::clone(value));
    }
}

impl<M: Steps, C: Held<Value = Arc<M>>> Stage for BaseSlot<M, C> {
    fn publish_pending(&self) {
        if let Some(map) = self.base.take_staged() {
            self.cell.replace(&map);
        }
    }
}

/// The bases of the project's tempo and meter.
#[derive(Clone, Debug)]
pub(crate) struct Bases {
    pub(crate) tempo: Base<TempoMap>,
    pub(crate) meter: Base<MeterMap>,
}

impl Bases {
    /// Bases holding the maps `settings` publishes now.
    pub(crate) fn of(settings: &TransportSettings) -> Self {
        Self {
            tempo: Base::new(settings.tempo_map()),
            meter: Base::new(settings.meter_map()),
        }
    }

    /// Drops the steps a follower laid and publishes the base again, for the
    /// maps it stepped.
    #[cfg(feature = "midi")]
    pub(crate) fn release(
        &self,
        settings: &TransportSettings,
        meter: &tutti_timeline::ProjectMeter,
    ) {
        if let Some(tempo) = self.tempo.release() {
            settings.set_tempo_map(tempo);
        }
        if let Some(meter_map) = self.meter.release() {
            meter.set_meter(meter_map);
        }
    }
}

#[cfg(all(test, feature = "midi"))]
mod tests {
    use super::*;
    use tutti_tempo::TempoPoint;
    use tutti_units::Bpm;

    fn bpm(value: f64) -> Bpm {
        Bpm::of(value)
    }

    fn base_of(points: &[(f64, f64)]) -> Arc<TempoMap> {
        let mut points = points.iter();
        let &(_, first) = points.next().expect("a first point");
        let mut builder = TempoMap::builder(bpm(first));
        for &(beat, tempo) in points {
            builder = builder.step_to(Beat::new(beat), bpm(tempo));
        }
        Arc::new(builder.build().expect("a map"))
    }

    fn recorded(base: &Base<TempoMap>) -> usize {
        base.0.lock().steps.len()
    }

    /// **A followed source that repeats a tempo leaves one recorded step**:
    /// 1000 equal tempos at increasing beats are one change of value, so the
    /// recorded steps, which every base write replays, stay bounded.
    #[test]
    fn a_repeated_followed_tempo_is_one_recorded_step() {
        let base = Base::new(base_of(&[(0.0, 120.0)]));
        for beat in 1..=1000 {
            base.push((Beat::new(f64::from(beat)), bpm(75.0)));
        }
        assert_eq!(recorded(&base), 1);
    }

    /// **A step drops the recorded steps at or after its beat**, and a value
    /// that equals the step before it is not recorded after the drop.
    #[test]
    fn a_step_truncates_the_recorded_steps_after_its_beat() {
        let base = Base::new(base_of(&[(0.0, 120.0)]));
        base.push((Beat::new(1.0), bpm(90.0)));
        base.push((Beat::new(3.0), bpm(60.0)));
        base.push((Beat::new(2.0), bpm(90.0)));
        assert_eq!(base.0.lock().steps, [(Beat::new(1.0), bpm(90.0))]);
        base.push((Beat::new(2.0), bpm(100.0)));
        assert_eq!(
            base.0.lock().steps,
            [(Beat::new(1.0), bpm(90.0)), (Beat::new(2.0), bpm(100.0))]
        );
    }

    /// **The laid map is the base before the first step, then the steps in
    /// order**, and a step's beat cuts the base's later points: a base of
    /// 100 bpm, 150 bpm at beat 1 and 200 bpm at beat 5 under steps of 75 bpm
    /// at beat 2 and 90 bpm at beat 4 is 100, 150, 75, 90 at beats 0, 1, 2, 4.
    #[test]
    fn the_laid_map_is_the_base_before_the_first_step_then_the_steps() {
        let base = Base::new(base_of(&[(0.0, 100.0), (1.0, 150.0), (5.0, 200.0)]));
        base.push((Beat::new(2.0), bpm(75.0)));
        base.push((Beat::new(4.0), bpm(90.0)));
        let laid = Base::laid(&base.0.lock());
        assert_eq!(
            laid.points(),
            [
                TempoPoint::step(Beat::new(0.0), bpm(100.0)),
                TempoPoint::step(Beat::new(1.0), bpm(150.0)),
                TempoPoint::step(Beat::new(2.0), bpm(75.0)),
                TempoPoint::step(Beat::new(4.0), bpm(90.0)),
            ]
        );
    }

    /// **A staged write is taken once**: `take_staged` answers the laid map
    /// after a `set`, and nothing the second time.
    #[test]
    fn a_staged_write_is_taken_once() {
        let base = Base::new(base_of(&[(0.0, 120.0)]));
        assert!(base.take_staged().is_none(), "nothing written");
        let written = base_of(&[(0.0, 90.0)]);
        base.set(Arc::clone(&written));
        assert!(Arc::ptr_eq(&base.take_staged().expect("staged"), &written));
        assert!(base.take_staged().is_none());
    }

    /// **A release with a staged write publishes the base and drops the
    /// steps**: the write made while following is the map released, and it is
    /// no longer staged. With no step a release answers nothing and leaves a
    /// staged write for the update.
    #[test]
    fn a_release_publishes_the_base_written_while_stepped() {
        let base = Base::new(base_of(&[(0.0, 120.0)]));
        base.set(base_of(&[(0.0, 60.0)]));
        assert!(base.release().is_none(), "no step, nothing to publish");
        assert!(base.take_staged().is_some(), "the write stays staged");

        base.push((Beat::new(1.0), bpm(75.0)));
        let written = base_of(&[(0.0, 90.0)]);
        base.set(Arc::clone(&written));
        assert!(Arc::ptr_eq(&base.release().expect("had a step"), &written));
        assert_eq!(recorded(&base), 0);
        assert!(base.take_staged().is_none(), "published by the release");
    }
}
