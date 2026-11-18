//! The operating-system calls of the plugin process edge on Windows, each
//! behind a safe function: the server's check that its host is still running
//! and its end without detach, and the host's peek at a named pipe.
//!
//! Each is a direct call with scalar or borrowed arguments; the policy around
//! it (what a failure to ask means) stays with the caller.

use std::os::windows::io::{AsRawHandle, BorrowedHandle};

/// Whether the process `pid` is still running.
///
/// **"Cannot determine" means alive** — only a positive answer counts as death.
/// `OpenProcess` returns null for *access denied* as readily as for *no such
/// process*, so a host at a higher integrity level than its plugin server (a
/// UAC-elevated DAW) would otherwise read as gone. `GetLastError` tells them
/// apart: `ERROR_INVALID_PARAMETER` is what Windows reports for a PID naming no
/// live process, the one unambiguous "it is gone".
#[must_use]
pub fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: `OpenProcess` takes only scalars. A null return means the process
    // is gone or inaccessible; the handle is closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // SAFETY: reads this thread's last-error value, set by the call above.
        let err = unsafe { GetLastError() };
        return err != ERROR_INVALID_PARAMETER;
    }
    // A process handle becomes signalled when the process exits, so a zero-length
    // wait is a liveness probe. Test for the *signalled* result specifically:
    // timing out means alive, but so does `WAIT_FAILED`, a failure to ask.
    // SAFETY: `handle` was just opened above and is still valid.
    let alive = unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0;
    // SAFETY: closing a handle opened above and not yet closed.
    unsafe { CloseHandle(handle) };
    alive
}

/// This process's parent PID, from the process table (Windows has no
/// `getppid`). `None` if the snapshot fails or this process is not in it.
#[must_use]
pub fn parent_process_id() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let me = std::process::id();
    // SAFETY: scalar arguments; the returned handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }

    // SAFETY: `PROCESSENTRY32W` is a plain C struct of integers and a `u16`
    // array, for which all-zero is a valid value.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    let mut found = None;
    // SAFETY: `snapshot` is valid and `entry` is a correctly sized, live
    // `PROCESSENTRY32W` for the duration of the walk.
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        if entry.th32ProcessID == me {
            found = Some(entry.th32ParentProcessID);
            break;
        }
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: closing a handle opened above and not yet closed.
    unsafe { CloseHandle(snapshot) };
    found
}

/// Ends this process at once (exit code 1), without running a plugin library's
/// detach or any other thread's cleanup (`TerminateProcess` on the current
/// process), for a server whose host is gone: a plain `exit` would run every
/// loaded library's detach routine while other threads are killed mid-call.
/// Aborts if the OS refuses.
pub fn terminate_process() -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

    // SAFETY: both calls take only scalars; `GetCurrentProcess` returns a
    // pseudo-handle that names this process and needs no close.
    unsafe { TerminateProcess(GetCurrentProcess(), 1) };
    std::process::abort()
}

/// Bytes the named pipe can hand over right now, without blocking or
/// consuming them (`PeekNamedPipe` as a pure size query).
///
/// `Ok(0)` means "nothing yet", never end of stream. A closed peer makes this
/// fail (`ERROR_BROKEN_PIPE`).
///
/// # Errors
///
/// The OS error of `PeekNamedPipe`.
pub fn pipe_bytes_available(pipe: BorrowedHandle<'_>) -> std::io::Result<u32> {
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let mut available: u32 = 0;
    // SAFETY: `pipe` is borrowed for this call. The only out-pointer is
    // `available`, a live local; every other buffer argument is null, which is
    // what makes this a pure size query: nothing is copied out of the pipe and
    // the stream position does not move.
    let ok = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(available)
}
