//! Windows: nothing to do.

use std::process::Command;

/// Nothing to set on Windows: a handle is inherited only when it is marked
/// inheritable and listed, and std lists the three standard handles alone.
pub fn inherit_only_stdio(_command: &mut Command) {}
