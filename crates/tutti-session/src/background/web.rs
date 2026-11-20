//! The web starts no threads: only the caller's drive runs the background queue.

use std::sync::mpsc::Receiver;

/// Takes what `rx` has now: a page cannot block, so the wait is the caller's loop of
/// drives and updates.
pub(crate) fn wait<T>(rx: &Receiver<T>) -> Option<T> {
    rx.try_recv().ok()
}

/// No value is ever held: a web page has no thread to start.
#[derive(Debug)]
pub(super) struct Threads;

impl Threads {
    /// No thread.
    pub(super) fn new() -> Self {
        Self
    }

    /// Always: nothing can start.
    pub(super) fn is_empty(&self) -> bool {
        true
    }
}
