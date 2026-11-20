//! The web starts no threads: `update` drives the background queue.

use std::io;

use tutti_platform::Executor;

/// What drives the background queue between updates: only `update` on the web.
pub(crate) struct Drive;

impl Drive {
    /// The web's default: no thread starts, and `update` drives `executor`.
    pub(crate) fn background(_executor: &Executor) -> io::Result<Self> {
        Ok(Self)
    }

    /// Nothing but `update` drives it.
    pub(crate) fn update() -> Self {
        Self
    }

    /// Never: a page has no threads.
    pub(crate) fn runs_itself(&self) -> bool {
        false
    }
}
