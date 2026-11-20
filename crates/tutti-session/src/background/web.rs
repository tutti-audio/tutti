//! The web starts no threads: only the caller's drive runs the background queue.

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
