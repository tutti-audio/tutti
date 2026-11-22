//! Unix: a leader starts as the leader of a process group of its own, and every
//! helper inherits descriptors 0, 1 and 2 only.

use super::{Spawn, SpawnError};
use crate::Reaped;
use crate::inherit::inherit_only_stdio;
use crate::lifetime::platform::own_group;
use std::process::Command;

/// Starts `command` as `how` says, with the standard descriptors and nothing
/// else of the host's, and holds the process.
///
/// # Errors
///
/// [`SpawnError::Start`] when the OS refuses the spawn.
pub fn spawn(mut command: Command, how: Spawn) -> Result<Reaped, SpawnError> {
    if how == Spawn::Leader {
        own_group(&mut command);
    }
    inherit_only_stdio(&mut command);
    let child = command.spawn().map_err(SpawnError::Start)?;
    Ok(match how {
        Spawn::Leader => Reaped::leading_group(child),
        Spawn::Helper => Reaped::new(child),
    })
}
