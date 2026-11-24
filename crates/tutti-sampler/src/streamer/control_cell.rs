//! [`ControlCell`], a published value the control side asks the streamer for.

use std::sync::Arc;

use tutti_rt::{Publisher, Reader};

use tutti_rt::Bell;

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
pub struct ControlCell<T> {
    cell: Publisher<T>,
    /// The streamer to wake after a publish; `None` for a resident voice's.
    bell: Option<Arc<Bell>>,
}

impl<T> ControlCell<T> {
    /// A cell holding `value`, wired to no streamer: a publish rings nothing.
    /// Only a stream's own cells ring its streamer.
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
            bell.ring();
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

/// The cell's own [`reclaim`](ControlCell::reclaim), as a published entry
/// sweeps it.
impl<T> tutti_rt::Reclaim for ControlCell<T> {
    fn reclaim(&self) -> usize {
        Self::reclaim(self)
    }
}

/// A voice's loop or direction, read and written as the value itself: a
/// publish that differs wakes the streamer of a streamed voice.
impl<T: Copy + PartialEq + Send + Sync + 'static> tutti_graph::Published for ControlCell<T> {
    type Value = T;

    fn current(&self) -> T {
        *Self::current(self)
    }

    fn replace(&self, value: &T) {
        if *Self::current(self) != *value {
            self.publish(*value);
        }
    }
}

impl<T> Clone for ControlCell<T> {
    fn clone(&self) -> Self {
        Self {
            cell: self.cell.clone(),
            bell: self.bell.clone(),
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ControlCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlCell")
            .field("cell", &self.cell)
            .field("wakes_streamer", &self.bell.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A publish to a streamed cell rings the streamer's bell, and one to a
    /// resident cell does not.**
    #[test]
    fn a_publish_to_a_streamed_cell_rings_the_streamer() {
        use std::future::Future;
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};

        let executor = tutti_platform::Executor::new();

        let bells = tutti_rt::Bells::new(&executor).expect("the bells' tick task");
        let (bell, mut listener) = bells.bell();
        let streamed = ControlCell::ringing(0u8, Arc::new(bell));
        let resident = ControlCell::new(0u8);
        let mut cx = Context::from_waker(Waker::noop());
        resident.publish(1);
        assert_eq!(
            pin!(listener.rung()).poll(&mut cx),
            Poll::Pending,
            "a resident voice's cell rings nothing"
        );
        streamed.publish(1);
        assert_eq!(
            pin!(listener.rung()).poll(&mut cx),
            Poll::Ready(()),
            "the streamer has a request to take"
        );
        assert_eq!((streamed.seq(), *streamed.current()), (1, 1));
    }
}
