//! [`Alive`]: whether a reader handle still exists, as the editor asks it.

use std::sync::{Arc, Weak};

/// A reader's presence: held by the reader, watched by the editor, which
/// drops the tap or meter once the reader is gone.
///
/// A reader's presence is not a writer's: a counter's `is_closed` answers
/// whether its writer dropped, and the editor holds the writer.
#[derive(Debug, Default)]
pub(crate) struct Alive(Arc<()>);

impl Alive {
    /// The handle a reader's `watch` returns: its strong count is zero once
    /// the reader is dropped.
    pub(crate) fn watch(&self) -> Weak<()> {
        Arc::downgrade(&self.0)
    }
}
