//! [`Alive`]: whether a handle still exists, as its watcher asks it.

use std::sync::{Arc, Weak};

/// A handle's presence: held by the handle, watched by a control-side owner.
/// The editor drops a tap or meter once its reader is gone, and an event
/// inlet's counts say whether its writer is.
///
/// A reader's presence is not a counter writer's: a counter's `is_closed`
/// answers whether its writer dropped, and the editor holds the writer.
#[derive(Debug, Default)]
pub(crate) struct Alive(Arc<()>);

impl Alive {
    /// The handle a reader's `watch` returns: its strong count is zero once
    /// the reader is dropped.
    pub(crate) fn watch(&self) -> Weak<()> {
        Arc::downgrade(&self.0)
    }
}
