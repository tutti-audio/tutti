//! The tick where no thread can start: the caller's `Bells::tick` runs the pass.

use std::io;

use tutti_platform::Executor;

use super::{Arc, Parker, Shared};

pub(super) struct Tick;

impl Tick {
    /// The web ticks inline: the executor is not used.
    pub(super) fn start(
        _shared: &Arc<Shared>,
        _parker: Parker,
        _executor: &Executor,
    ) -> io::Result<Self> {
        Ok(Self)
    }

    pub(super) fn run(&self, shared: &Shared) {
        shared.pass();
    }

    /// No thread: the drop runs the last pass itself.
    pub(super) fn shut(&self, shared: &Shared) {
        shared.last_pass();
    }

    #[cfg(any(test, loom))]
    pub(super) fn none() -> Self {
        Self
    }
}
