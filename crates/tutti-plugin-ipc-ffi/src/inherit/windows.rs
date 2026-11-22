//! Windows: nothing is done yet.

use std::process::Command;

/// Does nothing, and the host's inheritable handles are not yet kept from the
/// child. std 1.93 starts a process with `bInheritHandles` true and no handle
/// list, so every handle of the host marked inheritable reaches it. std's own
/// handles are made non-inheritable, but a CRT `_open` without `O_NOINHERIT`, a
/// plugin or a driver DLL can leave inheritable ones. Closing this takes a
/// hand-written CreateProcessW with `STARTUPINFOEX` and a
/// PROC_THREAD_ATTRIBUTE_HANDLE_LIST of the standard handles
/// (CommandExt::inherit_handles is unstable, rust-lang/rust#146407).
pub fn inherit_only_stdio(_command: &mut Command) {}
