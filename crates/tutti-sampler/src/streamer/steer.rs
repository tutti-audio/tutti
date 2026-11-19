//! [`SteerCell`], a published value the control side asks the streamer for.

use std::sync::Arc;

use tutti_rt::{Publisher, Reader};

use super::bell::Bell;

/// A published value that a voice and, for a streamed one, the
/// streamer read: the loop or direction a stream is asked for.
///
/// A cell of a streamed voice rings the streamer's bell when it is published
/// to, so the streamer takes the request on its next cycle without waiting
/// for its backstop. A cell of a resident voice rings nothing.
///
/// The control side publishes and reclaims ([`reclaim`](Self::reclaim), from a
/// tick it already has); every reader is lent the value once per block or
/// cycle.
pub struct SteerCell<T> {
    cell: Publisher<T>,
    /// The streamer to wake after a publish; `None` for a resident voice's.
    bell: Option<Arc<Bell>>,
}

impl<T> SteerCell<T> {
    /// A cell holding `value`, with no streamer behind it.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            cell: Publisher::new(value),
            bell: None,
        }
    }

    /// A cell holding `value` whose publishes wake the streamer `bell` rings.
    pub(crate) fn ringing(value: T, bell: Arc<Bell>) -> Self {
        Self {
            cell: Publisher::new(value),
            bell: Some(bell),
        }
    }

    /// Publishes `value` as the one in force, and wakes the streamer when the
    /// cell has one. Control thread.
    pub fn publish(&self, value: impl Into<Arc<T>>) {
        self.cell.publish(value);
        if let Some(bell) = &self.bell {
            bell.ring_steer();
        }
    }

    /// The value in force: an owning read, control side.
    #[must_use]
    pub fn current(&self) -> Arc<T> {
        self.cell.current()
    }

    /// How many values have been published.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.cell.seq()
    }

    /// A reader of the cell, registered here (control side) for the thread
    /// that will lend it.
    pub fn reader(&self) -> Reader<T> {
        self.cell.reader()
    }

    /// Frees the values replaced and no reader holds. Control thread.
    pub fn reclaim(&self) -> usize {
        self.cell.reclaim()
    }
}

impl<T> Clone for SteerCell<T> {
    fn clone(&self) -> Self {
        Self {
            cell: self.cell.clone(),
            bell: self.bell.clone(),
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for SteerCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteerCell")
            .field("cell", &self.cell)
            .field("wakes_streamer", &self.bell.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A publish to a streamed cell leaves the streamer work to do, and one
    /// to a resident cell does not**: the bell's pending work is what its
    /// sleep returns on, so a parked streamer wakes without its backstop. The
    /// streamer's next cycle clears it.
    #[test]
    fn a_publish_to_a_streamed_cell_wakes_the_streamer() {
        let (bell, _sleeper) = Bell::new();
        let streamed = SteerCell::ringing(0u8, Arc::clone(&bell));
        let resident = SteerCell::new(0u8);
        assert!(!bell.has_work());
        resident.publish(1);
        assert!(!bell.has_work(), "a resident voice's cell rings nothing");
        streamed.publish(1);
        assert!(bell.has_work(), "the streamer has a request to take");
        bell.begin_cycle();
        assert!(!bell.has_work(), "taken by the cycle that began");
        assert_eq!((streamed.seq(), *streamed.current()), (1, 1));
    }

    /// **A parked streamer returns from its sleep for a publish, not for its
    /// backstop**: the sleep over a cell's bell, with a backstop of a minute,
    /// returns at once when a request was published, so it ends far short of
    /// the backstop (the bound only guards a hang).
    #[test]
    fn a_parked_streamer_wakes_for_a_publish_not_its_backstop() {
        use std::time::{Duration, Instant};
        let (bell, mut sleeper) = Bell::new();
        let cell = SteerCell::ringing(0u8, Arc::clone(&bell));
        cell.publish(1);
        let parked = Instant::now();
        bell.sleep(&mut sleeper, || false, Duration::from_secs(60));
        assert!(
            parked.elapsed() < Duration::from_secs(30),
            "the sleep ran to its backstop"
        );
    }
}
