//! Windows: the process is the handle `CreateProcessW` returned and its id.

use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::{Child, ExitStatus};
use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, INFINITE, TerminateProcess, WaitForSingleObject,
};

/// A process this one started, held by [`Reaped`](crate::Reaped) and nowhere
/// else: its id, a non-blocking status, a kill, a blocking wait, and its handle
/// closed when it drops.
#[derive(Debug)]
pub struct Process {
    handle: OwnedHandle,
    id: u32,
}

impl From<Child> for Process {
    fn from(child: Child) -> Self {
        let id = child.id();
        Self {
            handle: child.into(),
            id,
        }
    }
}

impl Process {
    /// The process `id` that `handle` (with the right to wait, query and
    /// terminate) names.
    pub(crate) fn new(handle: OwnedHandle, id: u32) -> Self {
        Self { handle, id }
    }

    /// The process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The status if the process has ended.
    ///
    /// # Errors
    ///
    /// The OS error of the look.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        // SAFETY: `handle` is an open process handle owned by `self`; the wait
        // takes no pointer.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => self.exit_status().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// Kills the process; one that has ended already is `Ok`.
    ///
    /// # Errors
    ///
    /// The OS refused the kill.
    pub fn kill(&mut self) -> std::io::Result<()> {
        // SAFETY: `handle` is an open process handle owned by `self`; the exit
        // code is a scalar.
        if unsafe { TerminateProcess(self.handle.as_raw_handle(), 1) } != 0 {
            return Ok(());
        }
        let refused = std::io::Error::last_os_error();
        // An ended process refuses the kill with access denied.
        match self.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => Err(refused),
        }
    }

    /// Waits for the process to end, unbounded.
    ///
    /// # Errors
    ///
    /// The OS error of the wait.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        // SAFETY: `handle` is an open process handle owned by `self`; the wait
        // takes no pointer.
        if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), INFINITE) } == WAIT_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        self.exit_status()
    }

    /// The process as the OS wakes for its end ([`ExitWait`](crate::ExitWait)).
    pub(crate) fn as_handle(&self) -> BorrowedHandle<'_> {
        std::os::windows::io::AsHandle::as_handle(&self.handle)
    }

    /// The process as a raw handle, borrowed from `self`.
    pub(crate) fn as_raw_handle(&self) -> std::os::windows::raw::HANDLE {
        self.handle.as_raw_handle()
    }

    /// The exit code of a process known to have ended.
    fn exit_status(&self) -> std::io::Result<ExitStatus> {
        let mut code = 0;
        // SAFETY: `handle` is an open process handle owned by `self`, and
        // `code` is a live out-parameter.
        if unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ExitStatus::from_raw(code))
    }
}
