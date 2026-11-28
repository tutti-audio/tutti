//! [`Gesture`]: writes an app makes while a control is held, one gesture to a watcher.
//!
//! A gesture writes through the one batch path of a `set` and tells the one feed
//! what landed, under one [`GestureId`]; it adds no write path of its own. While it
//! is open its changes are not ended. Dropping it lands the end of every target it
//! moved as one landing, so a watcher's read sees all the ends or none.
//!
//! The gesture also tells each target each step of its hold through
//! `Store::gesture_step`: `GestureStep::Begin` at open, `Wrote` for each value
//! stored, and `End` at drop, moved or not. A plugin's parameter tells the
//! plugin (`PluginHandle::edit`); every other target has no one to tell.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Weak};

use tutti_rt::Mutex;

use super::error::{ValueError, ValueErrorKind};
use super::state::{Params, ValueState, whole_call};
use super::target::TargetId;
use super::watcher::{ChangedBy, Erased, Feed, GestureId, Grouping, IntoTargets, Stored};
use crate::AppKey;

/// What lands a gesture's end once the value lock is free.
pub(crate) type Ending = Box<dyn FnOnce(&Params, &Feed) + Send>;

/// The ends that could not land because the dropping thread held the value
/// lock (a [`Gesture`] dropped inside a `modify`): the call that holds the lock
/// lands them after its own batch.
#[derive(Default)]
pub(crate) struct Deferred(Mutex<Vec<Ending>>);

impl Deferred {
    pub(super) fn push(&self, ending: Ending) {
        self.0.lock().push(ending);
    }

    /// Lands every end queued, in the order they were queued.
    pub(super) fn land(&self, params: &Params, feed: &Feed) {
        let endings = std::mem::take(&mut *self.0.lock());
        for ending in endings {
            ending(params, feed);
        }
    }
}

/// A gesture over a set of targets: the writes of one drag, which a watcher
/// reads as one [`GestureId`] and an undo steps over once. Owned and `Send`, not
/// `Clone`.
///
/// [`set`](Self::set) takes an absolute value for each target the gesture was
/// opened over, in the shape it was opened with, so a target outside it cannot be
/// written through it. Its changes reach a watcher not ended; dropping the gesture
/// ends them, one change per target it moved. A gesture that moved nothing (a
/// click) records nothing. A write to one of its targets from elsewhere is its own
/// ungestured change, and leaves the gesture's base where it was.
///
/// The gesture reads the value lock like any call: dropping it inside a `modify`
/// is fine, and its end lands with that call's batch.
///
/// A plugin's parameter hears the hold: begin at open, each value, end at drop,
/// even for a click that moved nothing. An end dropped off the main thread
/// reaches a VST3 plugin at the next main-thread update.
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::Gesture<&'static str, tutti_session::TempoTarget>>();
/// ```
///
/// ```compile_fail
/// fn clone<T: Clone>() {}
/// clone::<tutti_session::Gesture<&'static str, tutti_session::TempoTarget>>();
/// ```
pub struct Gesture<K: AppKey, W: IntoTargets> {
    state: Weak<ValueState<K>>,
    id: GestureId,
    /// The targets, `Some` until the drop moves them into the end.
    targets: Option<W>,
    /// The base each target moved from, at the gesture's first change to it.
    moved: BTreeMap<TargetId, Erased>,
}

impl<K: AppKey, W: IntoTargets> Gesture<K, W> {
    pub(super) fn new(state: &Arc<ValueState<K>>, id: GestureId, targets: W) -> Self {
        Self {
            state: Arc::downgrade(state),
            id,
            targets: Some(targets),
            moved: BTreeMap::new(),
        }
    }

    /// The id every change of this gesture carries.
    pub fn id(&self) -> GestureId {
        self.id
    }

    /// Writes an absolute value to each target, in the shape the gesture was
    /// opened with, as one batch of a `set`: every value is checked before any
    /// lands. A value equal to the target's base is no change. A `None` of an
    /// `Option` writes nothing, and a `Vec` takes exactly one value per target.
    ///
    /// # Errors
    ///
    /// As [`Session::set`](crate::Session::set): a [`ValueError`] whose `at` is
    /// the first failing item's position, [`ValueErrorKind::NoSuchTarget`] for a
    /// target removed since the gesture opened,
    /// [`ValueErrorKind::Mismatched`] for a `Vec` of values that is not one
    /// per target, and [`ValueErrorKind::Closed`] once the session is gone.
    pub fn set(&mut self, values: W::Values) -> Result<(), ValueError<K>> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| whole_call(ValueErrorKind::Closed))?;
        let Some(targets) = self.targets.as_ref() else {
            return Ok(());
        };
        for (id, base) in state.write_gesture(self.id, targets, values)? {
            self.moved.entry(id).or_insert(base);
        }
        Ok(())
    }
}

impl<K: AppKey, W: IntoTargets> Drop for Gesture<K, W> {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(targets) = self.targets.take() else {
            return;
        };
        let (id, mut moved) = (self.id, std::mem::take(&mut self.moved));
        state.finish(Box::new(move |params, feed| {
            // The hold ends whether or not the gesture moved, and before the
            // feed lands the end: an end never overtakes a later begin.
            targets.end(params);
            if moved.is_empty() {
                return;
            }
            let mut out = Vec::new();
            targets.ended(params, &mut moved, &mut out);
            feed.land(params, Stored::new(out, ChangedBy::App), Grouping::End(id));
        }));
    }
}

impl<K: AppKey, W: IntoTargets> fmt::Debug for Gesture<K, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gesture")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl<K: AppKey> ValueState<K> {
    /// A gesture over `targets`, which are all there, with a new id.
    pub(crate) fn gesture<W: IntoTargets>(
        self: &Arc<Self>,
        targets: W,
    ) -> Result<Gesture<K, W>, ValueError<K>> {
        let held = self.enter()?;
        let (mut ids, mut at) = (Vec::new(), 0);
        targets
            .register(held.params(), &mut at, &mut ids)
            .map_err(super::state::error)?;
        let id = self.feed().open();
        targets.begin(held.params());
        Ok(Gesture::new(self, id, targets))
    }

    /// Stages `values` over `targets` as a batch, lands what moved as changes
    /// of the open gesture `id`, and answers the base of each target that moved.
    fn write_gesture<W: IntoTargets>(
        &self,
        id: GestureId,
        targets: &W,
        values: W::Values,
    ) -> Result<Vec<(TargetId, Erased)>, ValueError<K>> {
        let held = self.enter()?;
        let mut batch = super::batch::Batch::new(held.params());
        let wrote = values.clone();
        targets
            .stage(values, &mut batch)
            .map_err(super::state::error)?;
        let before = self.feed().capture_before(held.params(), id);
        let stored = batch.store();
        targets.wrote(held.params(), &wrote);
        let bases = stored.bases().collect();
        self.feed()
            .land(held.params(), stored, Grouping::Open(id, before));
        Ok(bases)
    }
}
