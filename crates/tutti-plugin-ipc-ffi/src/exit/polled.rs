//! A platform with no wake for a child's end: the wait sleeps.

use std::process::Child;
use std::time::Duration;

#[derive(Debug)]
pub(super) struct Imp;

impl Imp {
    pub(super) fn of(_child: &Child) -> Self {
        Self
    }

    pub(super) fn wait(&self, timeout: Duration) -> bool {
        std::thread::sleep(timeout.min(super::NAP));
        true
    }
}
