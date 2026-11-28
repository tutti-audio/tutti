//! The writes a flush keeps for the next block: [`HeldWrites`].

use tutti_plugin_types::epoch_before;
use tutti_units::{Normalized, ParamId};

/// The latest value of each `REQUIRES_PROCESS` parameter a `params.flush` was
/// asked to deliver, kept for the next block's `process`.
///
/// CLAP takes such a change only in `process`, in event order, while the plugin
/// is active. An entry holds the slots' epoch its value was checked against: a
/// [`reset`](tutti_plugin_types::EditWriter::reset) made after it ends the
/// epoch, and the entry is dropped, never sent. The room is one entry per
/// declared parameter, fixed at activation, so keeping one never allocates;
/// entries stay sorted by id, so a lookup is a binary search.
#[derive(Debug)]
pub(crate) struct HeldWrites {
    entries: Vec<(ParamId, Normalized, u32)>,
}

impl HeldWrites {
    /// Room for `declared` parameters.
    pub(crate) fn with_room(declared: usize) -> Self {
        Self {
            entries: Vec::with_capacity(declared),
        }
    }

    /// Keeps `value` as the latest of `id`, checked against `epoch`: `false`
    /// when there is no room for a parameter not yet held, which a table
    /// with more parameters than the activation declared would reach.
    pub(crate) fn keep(&mut self, id: ParamId, value: Normalized, epoch: u32) -> bool {
        match self.entries.binary_search_by_key(&id, |entry| entry.0) {
            Ok(at) => {
                self.entries[at] = (id, value, epoch);
                true
            }
            Err(at) if self.entries.len() < self.entries.capacity() => {
                self.entries.insert(at, (id, value, epoch));
                true
            }
            Err(_) => false,
        }
    }

    /// Drops what an epoch after the one it was checked against made stale.
    pub(crate) fn drop_stale(&mut self, now: u32) {
        self.entries
            .retain(|&(_, _, epoch)| !epoch_before(epoch, now));
    }

    /// Forgets the value of `id`: a newer write of it takes its place.
    pub(crate) fn supersede(&mut self, id: ParamId) {
        if let Ok(at) = self.entries.binary_search_by_key(&id, |entry| entry.0) {
            self.entries.remove(at);
        }
    }

    /// Hands `apply` each entry still current at epoch `now`, in id order, and
    /// empties the list.
    pub(crate) fn take(&mut self, now: u32, mut apply: impl FnMut(ParamId, Normalized)) {
        for (id, value, epoch) in self.entries.drain(..) {
            if !epoch_before(epoch, now) {
                apply(id, value);
            }
        }
    }

    /// Whether an entry is still current at epoch `now`.
    pub(crate) fn any_current(&self, now: u32) -> bool {
        self.entries
            .iter()
            .any(|&(_, _, epoch)| !epoch_before(epoch, now))
    }

    /// Whether nothing is held.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> ParamId {
        ParamId::new(n)
    }

    fn taken(held: &mut HeldWrites, now: u32) -> Vec<(ParamId, f64)> {
        let mut out = Vec::new();
        held.take(now, |id, value| out.push((id, value.get())));
        out
    }

    /// **A parameter is held once, at its newest value, and the list is
    /// handed in id order.**
    #[test]
    fn the_newest_value_of_each_parameter_is_held_in_id_order() {
        let mut held = HeldWrites::with_room(3);
        assert!(held.keep(id(9), Normalized::of(0.25), 0));
        assert!(held.keep(id(2), Normalized::of(0.5), 0));
        assert!(held.keep(id(9), Normalized::of(0.75), 0));
        assert_eq!(taken(&mut held, 0), [(id(2), 0.5), (id(9), 0.75)]);
        assert!(held.is_empty(), "taken");
    }

    /// **A parameter past the room is refused, and one already held is
    /// replaced in it.**
    #[test]
    fn a_parameter_past_the_room_is_refused() {
        let mut held = HeldWrites::with_room(1);
        assert!(held.keep(id(1), Normalized::of(0.25), 0));
        assert!(!held.keep(id(2), Normalized::of(0.5), 0));
        assert!(held.keep(id(1), Normalized::of(0.5), 0));
        assert_eq!(taken(&mut held, 0), [(id(1), 0.5)]);
    }
}
