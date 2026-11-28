//! The host's own gestures toward a plugin, as an ordered list: [`HostEdits`].
//!
//! A host that holds a parameter tells the plugin so in the order the spec
//! gives: [`Begin`](ParamEdit::Begin), the values ([`Change`](ParamEdit::Change)),
//! then [`End`](ParamEdit::End). VST3 takes it as `beginEditFromHost`,
//! `setParamNormalized`, `endEditFromHost` on the main thread, AU as gesture
//! notifications around `AudioUnitSetParameter`; CLAP and VST2 define none
//! ([`EditOutcome::NotDefined`]).
//!
//! The senders are any thread and the drain is the plugin's main thread, so
//! the list is a control-side lock around a bounded, coalesced FIFO.

use std::collections::VecDeque;
use std::sync::Arc;

use tutti_rt::{Mutex, TallyWriter, tally};
use tutti_units::ParamId;

use crate::ParamEdit;

/// What a plugin's format does with a host gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// The edit was queued for the plugin's main thread.
    Queued,
    /// The format defines no host gesture (CLAP `ext/params.h` scenario II,
    /// VST2: the gesture goes from plugin to host only).
    NotDefined,
}

/// The ordered list of the host's gestures toward one plugin, from any thread
/// to its main thread.
///
/// - FIFO: the drain sees `Begin`, the values and `End` in the order posted.
/// - A `Change` is queued only while its id has an open `Begin`, and replaces
///   that id's earlier `Change` since the `Begin`.
/// - `Begin` and `End` are never dropped; an `End` whose id is not open is
///   not queued (a replacement plugin never heard the `Begin`).
/// - A `Change` that finds the list holding two more than one per held id is
///   counted in [`lost`](Self::lost) and not queued; nothing waits.
#[derive(Debug)]
pub struct HostEdits {
    list: Mutex<List>,
    lost: TallyWriter<u64>,
}

#[derive(Debug, Default)]
struct List {
    queued: VecDeque<ParamEdit>,
    /// The ids with an open `Begin`, one entry per `Begin`.
    held: Vec<ParamId>,
}

impl HostEdits {
    /// An empty list.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            list: Mutex::new(List::default()),
            lost: tally::<u64>().writer(),
        })
    }

    /// Queues `edit` under the rules above. Never waits for the main thread.
    pub fn post(&self, edit: ParamEdit) {
        let mut list = self.list.lock();
        let id = edit.id();
        match edit {
            ParamEdit::Begin(_) => {
                list.held.push(id);
                list.queued.push_back(edit);
            }
            ParamEdit::End(_) => {
                if let Some(at) = list.held.iter().position(|held| *held == id) {
                    list.held.remove(at);
                    list.queued.push_back(edit);
                }
            }
            ParamEdit::Change { .. } => {
                if !list.held.contains(&id) {
                    return;
                }
                let earlier = list
                    .queued
                    .iter_mut()
                    .rev()
                    .find(|queued| queued.id() == id);
                match earlier {
                    Some(slot @ ParamEdit::Change { .. }) => *slot = edit,
                    _ => {
                        let changes = list
                            .queued
                            .iter()
                            .filter(|queued| matches!(queued, ParamEdit::Change { .. }))
                            .count();
                        if changes >= 2 + list.held.len() {
                            self.lost.add(1);
                        } else {
                            list.queued.push_back(edit);
                        }
                    }
                }
            }
        }
    }

    /// Hands each queued edit to `apply`, oldest first. What is posted
    /// meanwhile waits for the next call.
    pub fn drain(&self, mut apply: impl FnMut(ParamEdit)) {
        let taken = std::mem::take(&mut self.list.lock().queued);
        for edit in taken {
            apply(edit);
        }
    }

    /// Hands queued edits to `apply`, oldest first, until it answers `false`:
    /// that edit and the rest stay queued, in order, for the next call. For a
    /// receiver that can be full.
    pub fn drain_while(&self, mut apply: impl FnMut(ParamEdit) -> bool) {
        let mut list = self.list.lock();
        while let Some(edit) = list.queued.pop_front() {
            if !apply(edit) {
                list.queued.push_front(edit);
                break;
            }
        }
    }

    /// Forgets every open gesture and every queued edit: the plugin they were
    /// for is gone, and its replacement never heard the begin.
    pub fn reset(&self) {
        *self.list.lock() = List::default();
    }

    /// Whether the list holds `id`: its gesture is open, or an edit of it
    /// waits for the drain. A host that also writes the plugin's controller
    /// another way leaves `id` to the list meanwhile, so the controller hears
    /// one ordered stream.
    #[must_use]
    pub fn holds(&self, id: ParamId) -> bool {
        let list = self.list.lock();
        list.held.contains(&id) || list.queued.iter().any(|queued| queued.id() == id)
    }

    /// How many `Change`s were not queued for want of room, over the list's
    /// life.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost.get()
    }
}

#[cfg(test)]
mod tests {
    use tutti_units::Normalized;

    use super::*;

    fn drained(edits: &HostEdits) -> Vec<ParamEdit> {
        let mut out = Vec::new();
        edits.drain(|edit| out.push(edit));
        out
    }

    fn change(id: u32, value: f64) -> ParamEdit {
        ParamEdit::change(ParamId::new(id), Normalized::of(value))
    }

    /// The drain sees the spec sequence, and a later value of an open id
    /// replaces an earlier one.
    #[test]
    fn begin_the_latest_value_then_end_in_order() {
        let edits = HostEdits::new();
        let id = ParamId::new(7);
        edits.post(ParamEdit::Begin(id));
        edits.post(change(7, 0.1));
        edits.post(change(7, 0.2));
        edits.post(change(7, 0.3));
        edits.post(ParamEdit::End(id));
        assert_eq!(
            drained(&edits),
            [ParamEdit::Begin(id), change(7, 0.3), ParamEdit::End(id)]
        );
    }

    /// A value with no open begin, and an end that closes nothing, are not
    /// queued.
    #[test]
    fn a_change_without_a_begin_and_a_stray_end_are_not_queued() {
        let edits = HostEdits::new();
        edits.post(change(1, 0.5));
        edits.post(ParamEdit::End(ParamId::new(1)));
        assert_eq!(drained(&edits), []);
    }

    /// An id held across two drains keeps taking values: the second drain
    /// carries the value and the end.
    #[test]
    fn a_gesture_open_across_drains_keeps_its_hold() {
        let edits = HostEdits::new();
        let id = ParamId::new(2);
        edits.post(ParamEdit::Begin(id));
        assert_eq!(drained(&edits), [ParamEdit::Begin(id)]);
        edits.post(change(2, 0.4));
        edits.post(ParamEdit::End(id));
        assert_eq!(drained(&edits), [change(2, 0.4), ParamEdit::End(id)]);
    }

    /// A receiver that is full leaves the edit and the rest queued, in order.
    #[test]
    fn a_full_receiver_leaves_the_rest_queued_in_order() {
        let edits = HostEdits::new();
        let id = ParamId::new(5);
        edits.post(ParamEdit::Begin(id));
        edits.post(change(5, 0.5));
        edits.post(ParamEdit::End(id));
        let mut taken = Vec::new();
        edits.drain_while(|edit| {
            if taken.len() == 1 {
                return false;
            }
            taken.push(edit);
            true
        });
        assert_eq!(taken, [ParamEdit::Begin(id)]);
        assert_eq!(drained(&edits), [change(5, 0.5), ParamEdit::End(id)]);
    }

    /// A reset forgets the hold: the end that follows is not queued.
    #[test]
    fn a_reset_forgets_the_open_gestures() {
        let edits = HostEdits::new();
        let id = ParamId::new(6);
        edits.post(ParamEdit::Begin(id));
        edits.reset();
        edits.post(ParamEdit::End(id));
        assert_eq!(drained(&edits), []);
    }

    /// Begin and end are never dropped, whatever the number of ids held; a
    /// value of a second gesture on the same id does not take the first's.
    #[test]
    fn begins_and_ends_are_never_dropped() {
        let edits = HostEdits::new();
        let id = ParamId::new(3);
        for _ in 0..100 {
            edits.post(ParamEdit::Begin(id));
            edits.post(ParamEdit::End(id));
        }
        assert_eq!(drained(&edits).len(), 200);
        assert_eq!(edits.lost(), 0);
    }
}
