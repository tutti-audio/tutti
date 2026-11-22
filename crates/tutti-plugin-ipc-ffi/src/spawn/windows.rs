//! Windows: a leader is assigned to the host's kill-on-close job after its spawn.

use super::{Spawn, SpawnError};
use crate::Reaped;
use crate::lifetime::platform::bind_to_host;
use std::process::Command;

/// Starts `command` as `how` says and holds the process.
///
/// The host's inheritable handles are not yet kept from the child. std 1.93
/// starts a process with `bInheritHandles` true and no handle list, so every
/// handle of the host marked inheritable reaches it. std's own handles are made
/// non-inheritable, but a CRT `_open` without `O_NOINHERIT`, a plugin or a
/// driver DLL can leave inheritable ones. Closing this takes a hand-written
/// CreateProcessW with `STARTUPINFOEX` and a PROC_THREAD_ATTRIBUTE_HANDLE_LIST of
/// the standard handles (CommandExt::inherit_handles is unstable,
/// rust-lang/rust#146407).
///
/// # Errors
///
/// [`SpawnError::Start`] when the OS refuses the spawn, [`SpawnError::NotBound`]
/// when a leader cannot be assigned to the job (it is killed and reaped).
pub fn spawn(mut command: Command, how: Spawn) -> Result<Reaped, SpawnError> {
    let child = command.spawn().map_err(SpawnError::Start)?;
    if how == Spawn::Helper {
        return Ok(Reaped::new(child));
    }
    let bound = bind_to_host(&child);
    // On the refusal `reaped` drops: killed and reaped.
    let reaped = Reaped::leading_group(child);
    bound.map_err(SpawnError::NotBound)?;
    Ok(reaped)
}
