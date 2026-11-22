//! The sanitizer seam with the `rtsan` feature off: nothing to hold.

/// No sanitizer region.
#[derive(Debug)]
pub(super) struct Region;

impl Region {
    pub(super) const fn enter() -> Self {
        Self
    }
}

/// No error reporting to silence.
pub(super) struct Quiet;

impl Quiet {
    pub(super) const fn new() -> Self {
        Self
    }
}
