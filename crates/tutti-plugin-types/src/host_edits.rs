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
//! the list is a control-side lock around a coalesced FIFO, bounded by three
//! entries per parameter.

use std::collections::VecDeque;
use tutti_rt::Mutex;
use tutti_units::ParamId;

use crate::ParamEdit;

/// What a plugin's format does with a host gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
/// - A gesture that begins on an id whose last queued edit is its `End`
///   reopens it: the `End` goes, and the two gestures drain as one `Begin`,
///   the latest value, `End`. Both would have reached the plugin at the same
///   tick, so the plugin cannot tell them apart.
/// - A `Begin` on an id already open queues nothing, and only the `End` that
///   closes the last open hold is queued: the plugin hears one gesture.
/// - An `End` whose id is not open is not queued (a replacement plugin never
///   heard the `Begin`).
///
/// So each id holds at most `Begin`, `Change`, `End` between two drains: the
/// list is bounded by three entries per id with no refusal, and nothing waits.
#[derive(Debug, Default)]
pub struct HostEdits {
    list: Mutex<List>,
}

#[derive(Debug, Default)]
struct List {
    queued: VecDeque<ParamEdit>,
    /// The ids with an open gesture, each with how many holds are open on it.
    held: Vec<(ParamId, u32)>,
}

impl List {
    fn holds(&mut self, id: ParamId) -> Option<&mut u32> {
        self.held
            .iter_mut()
            .find(|(held, _)| *held == id)
            .map(|(_, count)| count)
    }

    /// The position of the last queued edit of `id`.
    fn last_of(&self, id: ParamId) -> Option<usize> {
        self.queued.iter().rposition(|queued| queued.id() == id)
    }
}

impl HostEdits {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `edit` under the rules above. Never waits for the main thread.
    pub fn post(&self, edit: ParamEdit) {
        let mut list = self.list.lock();
        let id = edit.id();
        match edit {
            ParamEdit::Begin(_) => {
                if let Some(count) = list.holds(id) {
                    *count += 1;
                    return;
                }
                list.held.push((id, 1));
                match list.last_of(id) {
                    Some(at) if matches!(list.queued[at], ParamEdit::End(_)) => {
                        list.queued.remove(at);
                    }
                    _ => list.queued.push_back(edit),
                }
            }
            ParamEdit::End(_) => {
                let Some(count) = list.holds(id) else {
                    return;
                };
                *count -= 1;
                if *count == 0 {
                    list.held.retain(|(held, _)| *held != id);
                    list.queued.push_back(edit);
                }
            }
            ParamEdit::Change { .. } => {
                if list.holds(id).is_none() {
                    return;
                }
                match list.last_of(id) {
                    Some(at) if matches!(list.queued[at], ParamEdit::Change { .. }) => {
                        list.queued[at] = edit;
                    }
                    _ => list.queued.push_back(edit),
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
    ///
    /// `apply` runs with the list locked, so it must not block: a poster waits
    /// for it.
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
        let mut list = self.list.lock();
        list.holds(id).is_some() || list.last_of(id).is_some()
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

    /// Back-to-back gestures on one id between two drains drain as one, with
    /// the last value: the list does not grow with the number of gestures.
    #[test]
    fn back_to_back_gestures_on_one_id_drain_as_one_with_the_last_value() {
        let edits = HostEdits::new();
        let id = ParamId::new(3);
        for n in 0..100 {
            edits.post(ParamEdit::Begin(id));
            edits.post(change(3, f64::from(n) / 100.0));
            edits.post(ParamEdit::End(id));
        }
        assert_eq!(
            drained(&edits),
            [ParamEdit::Begin(id), change(3, 0.99), ParamEdit::End(id)]
        );
    }

    /// Four clicks between two drains: the plugin ends at the last one's
    /// value, the value the host holds.
    #[test]
    fn the_last_gestures_value_is_never_lost() {
        let edits = HostEdits::new();
        let id = ParamId::new(4);
        for value in [0.1, 0.2, 0.3, 0.4] {
            edits.post(ParamEdit::Begin(id));
            edits.post(change(4, value));
            edits.post(ParamEdit::End(id));
        }
        assert_eq!(
            drained(&edits),
            [ParamEdit::Begin(id), change(4, 0.4), ParamEdit::End(id)]
        );
    }

    /// The same through a receiver that takes one edit per call, as the
    /// client's outbox drains into a full control queue: gestures posted while
    /// edits wait merge with them, and the last value arrives.
    #[test]
    fn a_full_receiver_still_delivers_the_last_value() {
        let edits = HostEdits::new();
        let id = ParamId::new(8);
        let mut taken = Vec::new();
        for value in [0.1, 0.2, 0.3, 0.4] {
            edits.post(ParamEdit::Begin(id));
            edits.post(change(8, value));
            edits.post(ParamEdit::End(id));
            let mut room = 1;
            edits.drain_while(|edit| {
                if room == 0 {
                    return false;
                }
                room -= 1;
                taken.push(edit);
                true
            });
        }
        edits.drain(|edit| taken.push(edit));
        assert_eq!(taken.last(), Some(&ParamEdit::End(id)));
        assert_eq!(
            taken
                .iter()
                .rev()
                .find(|e| matches!(e, ParamEdit::Change { .. })),
            Some(&change(8, 0.4)),
            "the plugin ends at the host's value"
        );
        let begins = taken
            .iter()
            .filter(|e| matches!(e, ParamEdit::Begin(_)))
            .count();
        let ends = taken
            .iter()
            .filter(|e| matches!(e, ParamEdit::End(_)))
            .count();
        assert_eq!(begins, ends, "every begin is ended");
    }

    /// Two holds open on one id at once reach the plugin as one gesture: the
    /// end goes only when the last hold closes.
    #[test]
    fn overlapping_holds_on_one_id_are_one_gesture() {
        let edits = HostEdits::new();
        let id = ParamId::new(9);
        edits.post(ParamEdit::Begin(id));
        edits.post(ParamEdit::Begin(id));
        edits.post(change(9, 0.5));
        edits.post(ParamEdit::End(id));
        assert_eq!(drained(&edits), [ParamEdit::Begin(id), change(9, 0.5)]);
        edits.post(ParamEdit::End(id));
        assert_eq!(drained(&edits), [ParamEdit::End(id)]);
    }
}
