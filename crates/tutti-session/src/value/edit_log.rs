//! [`EditLog`]: every landing as undo steps, drained by the app.
//!
//! A log is the second kind of subscriber to the one feed a [`Watcher`](crate::Watcher)
//! reads. For each target a landing moved it records an [`EditMark`] with the
//! saved values as they stood at that landing. Marks of one
//! [`GestureId`] are one step; a mark of no gesture is a step of
//! its own. The feed captures the saved values only while a log exists.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Weak};

use tutti_rt::Mutex;
use tutti_units::UndoSteps;

use super::saved::{Captured, SavedValues};
use super::state::ValueState;
use super::target::{AnyTarget, TargetId};
use super::watcher::{ChangedBy, GestureId};
use crate::AppKey;

/// One target a landing moved, as an undo step records it.
///
/// An app undoes a step by putting back the `after` of the step before it
/// (`session.set(&mark.after)`); a mark has no undo or redo data of its own.
/// A mark of [`ChangedBy::Restore`] is not an undo step: an app that restores
/// a saved map reads it as a step it need not offer to undo twice.
///
/// Writes the session makes itself (`ValueTable::write`, a MIDI clip played)
/// make no mark.
#[non_exhaustive]
pub struct EditMark<K> {
    /// The target that moved.
    pub target: AnyTarget<K>,
    /// Who moved it.
    pub by: ChangedBy,
    /// The gesture it belongs to: `None` for a call of one write. The marks of
    /// one gesture are one undo step.
    pub gesture: Option<GestureId>,
    /// Whether the gesture is over: a batch or a single write ends with its
    /// call, a [`Gesture`](crate::Gesture) when it is dropped.
    pub ended: bool,
    /// The saved values as they stood at the landing. For a mark of a gesture
    /// that is not ended, the gesture's targets are at their base before the
    /// first of its writes the log saw: a log made inside a gesture holds them
    /// at their value before the next write.
    pub after: SavedValues<K>,
}

impl<K> Clone for EditMark<K> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            by: self.by,
            gesture: self.gesture,
            ended: self.ended,
            after: self.after.clone(),
        }
    }
}

impl<K> fmt::Debug for EditMark<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditMark")
            .field("target", &self.target)
            .field("by", &self.by)
            .field("gesture", &self.gesture)
            .field("ended", &self.ended)
            .field("after", &self.after)
            .finish()
    }
}

/// A mark without the session's key type, as the feed hands it over.
pub(super) struct RawMark {
    pub(super) id: TargetId,
    pub(super) by: ChangedBy,
    pub(super) gesture: Option<GestureId>,
    pub(super) ended: bool,
    pub(super) after: Captured,
}

struct Step {
    gesture: Option<GestureId>,
    marks: Vec<RawMark>,
}

struct Steps {
    /// `None` keeps every step.
    cap: Option<UndoSteps>,
    steps: VecDeque<Step>,
    coalesced: UndoSteps,
}

/// A log's side of the feed: the steps landed since it was last drained.
pub(crate) struct LogSlot {
    steps: Mutex<Steps>,
}

impl LogSlot {
    /// Records one landing's marks, which share one gesture, then drops the
    /// oldest steps while there are more than the cap.
    pub(super) fn push(&self, marks: impl Iterator<Item = RawMark>) {
        let mut log = self.steps.lock();
        for mark in marks {
            let open = mark
                .gesture
                .and_then(|id| log.steps.iter().rposition(|s| s.gesture == Some(id)));
            match open {
                Some(at) => log.steps[at].marks.push(mark),
                None => log.steps.push_back(Step {
                    gesture: mark.gesture,
                    marks: vec![mark],
                }),
            }
        }
        log.coalesce();
    }
}

impl Steps {
    /// Drops the oldest step until `cap` remain, counting each: the `after` of
    /// every newer mark still holds the values a dropped step set.
    fn coalesce(&mut self) {
        while self.cap.is_some_and(|cap| self.held() > cap) {
            self.steps.pop_front();
            self.coalesced += UndoSteps::new(1);
        }
    }

    /// The undrained steps.
    fn held(&self) -> UndoSteps {
        UndoSteps::new(u64::try_from(self.steps.len()).unwrap_or(u64::MAX))
    }
}

/// The undo steps landed since the last [`drain`](Self::drain), in order.
///
/// Made by [`Session::edit_log`](crate::Session::edit_log) or
/// [`Values::edit_log`](crate::Values::edit_log). It records only what lands
/// after it is made; dropping it stops the recording. `Send`, not `Clone`: one
/// owner drains it.
///
/// Past [`cap`](Self::cap) undrained steps, the oldest step is dropped: its
/// marks go, and [`coalesced`](Self::coalesced) counts it. The `after` of every
/// newer mark still holds the values it set.
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::EditLog<&'static str>>();
/// ```
///
/// ```compile_fail
/// fn clone<T: Clone>() {}
/// clone::<tutti_session::EditLog<&'static str>>();
/// ```
pub struct EditLog<K: AppKey> {
    slot: Arc<LogSlot>,
    state: Weak<ValueState<K>>,
}

impl<K: AppKey> EditLog<K> {
    pub(super) fn new(state: &Arc<ValueState<K>>) -> Self {
        let slot = Arc::new(LogSlot {
            steps: Mutex::new(Steps {
                cap: None,
                steps: VecDeque::new(),
                coalesced: UndoSteps::ZERO,
            }),
        });
        state.feed().subscribe_log(&slot);
        Self {
            slot,
            state: Arc::downgrade(state),
        }
    }

    /// Bounds the undrained steps to `n`: past it, the oldest step is dropped
    /// and counted. A cap of zero keeps no step and only counts.
    #[must_use]
    pub fn cap(self, n: UndoSteps) -> Self {
        {
            let mut log = self.slot.steps.lock();
            log.cap = Some(n);
            log.coalesce();
        }
        self
    }

    /// The steps dropped past the cap so far, over the log's life.
    #[must_use]
    pub fn coalesced(&self) -> UndoSteps {
        self.slot.steps.lock().coalesced
    }

    /// Takes every mark landed since the last drain, oldest first, the marks
    /// of one step together. The batch is taken under the log's lock and
    /// iterated without it, so a `set` inside the loop does not wait.
    pub fn drain(&mut self) -> impl Iterator<Item = EditMark<K>> + use<K> {
        let taken = std::mem::take(&mut self.slot.steps.lock().steps);
        taken
            .into_iter()
            .flat_map(|step| step.marks)
            .map(|mark| EditMark {
                target: AnyTarget::of(mark.id),
                by: mark.by,
                gesture: mark.gesture,
                ended: mark.ended,
                after: SavedValues::of_captured(mark.after),
            })
    }
}

impl<K: AppKey> Drop for EditLog<K> {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.feed().unsubscribe_log(&self.slot);
        }
    }
}

impl<K: AppKey> fmt::Debug for EditLog<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditLog").finish_non_exhaustive()
    }
}

impl<K: AppKey> ValueState<K> {
    /// A log of every landing after this call. Subscribed under the value
    /// lock, so no landing falls between its creation and its subscription.
    pub(crate) fn edit_log(self: &Arc<Self>) -> Result<EditLog<K>, super::error::ValueError<K>> {
        let _held = self.enter()?;
        Ok(EditLog::new(self))
    }
}
