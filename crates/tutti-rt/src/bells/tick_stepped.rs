//! The tick under loom: no task, since `tutti-platform` has no executor there. The
//! model steps the body by hand through its `Ticker`.

use super::Shared;

pub(super) struct Tick;

impl Tick {
    /// The model's ticker runs the passes: nothing to do here.
    pub(super) fn run(&self, _shared: &Shared) {}

    /// The model's ticker runs the last pass: nothing to do here.
    pub(super) fn shut(&self, _shared: &Shared) {}

    pub(super) fn none() -> Self {
        Self
    }
}
