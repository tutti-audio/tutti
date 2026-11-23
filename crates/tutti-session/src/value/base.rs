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

    /// This map with `step` laid over it: the map before the step as it
    /// stands, the step, and nothing after. `None` when that changes nothing.
    fn stepped(&self, step: Self::Step) -> Option<Self>;
}

impl Steps for TempoMap {
    /// The beat a tempo is placed at, and the tempo.
    type Step = (Beat, Bpm);

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
        let mut map = Arc::clone(&state.base);
        for &step in &state.steps {
            if let Some(next) = map.stepped(step) {
                map = Arc::new(next);
            }
        }
        map
    }

    /// Records `step`, placed over the map the caller has published.
    #[cfg(feature = "midi")]
    pub(crate) fn push(&self, step: M::Step) {
        self.0.lock().steps.push(step);
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
            meter: Base::new(settings.project_meter().current()),
        }
    }
}
