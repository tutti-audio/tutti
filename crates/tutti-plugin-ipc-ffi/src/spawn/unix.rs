//! Unix: a leader starts as the leader of a process group of its own, a helper's
//! standard streams are `/dev/null`, and every helper inherits descriptors 0, 1
//! and 2 only.

use super::{Spawn, SpawnError};
use crate::Reaped;
use crate::inherit::inherit_only_stdio;
use crate::lifetime::platform::own_group;
use std::process::{Command, Stdio};

/// Starts `command` as `how` says, with the standard descriptors and nothing
/// else of the host's, and holds the process.
///
/// A [`Spawn::Helper`]'s `stdin`, `stdout` and `stderr` are `/dev/null` and a
/// [`Spawn::Leader`]'s `stdout` and `stderr` are the host's (the command's own
/// `stdout` and `stderr` are not read). A leader's `stdin` is the command's own,
/// the caller's channel to it.
///
/// # Errors
///
/// [`SpawnError::Start`] when the OS refuses the spawn.
pub fn spawn(mut command: Command, how: Spawn) -> Result<Reaped, SpawnError> {
    match how {
        Spawn::Leader => {
            own_group(&mut command);
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }
        Spawn::Helper => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        }
    }
    inherit_only_stdio(&mut command);
    let child = command.spawn().map_err(SpawnError::Start)?;
    Ok(match how {
        Spawn::Leader => Reaped::leading_group(child),
        Spawn::Helper => Reaped::new(child),
    })
}
