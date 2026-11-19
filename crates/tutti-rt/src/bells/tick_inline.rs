//! The tick where no thread can start: the caller's `Bells::tick` runs the pass.

use std::io;

use super::{Arc, Parker, Shared};

pub(super) struct Tick;

impl Tick {
    pub(super) fn start(_shared: &Arc<Shared>, _parker: Parker) -> io::Result<Self> {
        Ok(Self)
    }

    pub(super) fn run(&self, shared: &Shared) {
        shared.pass();
    }

    #[cfg(any(test, loom))]
    pub(super) fn none() -> Self {
        Self
    }
}
