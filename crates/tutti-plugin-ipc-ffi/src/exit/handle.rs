//! Windows: a process handle is signalled when the process ends.

use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::process::Child;
use std::time::Duration;

use super::Waited;
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

#[derive(Debug)]
pub(super) struct Imp {
    /// A duplicate of the child's handle, so the wait outlives no close of
    /// the original.
    handle: OwnedHandle,
}

impl Imp {
    /// `None` when the handle could not be duplicated.
    pub(super) fn of(child: &Child) -> Option<Self> {
        let handle = child.as_handle().try_clone_to_owned().ok()?;
        Some(Self { handle })
    }

    /// A watch on the process `pid` of any parent. `None` when it has ended
    /// already or cannot be opened.
    #[cfg(not(loom))]
    pub(super) fn of_pid(pid: u32) -> Option<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

        // SAFETY: `OpenProcess` takes only scalars; a non-null handle it
        // returns is owned here and closed by the `OwnedHandle`.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        (!handle.is_null()).then(|| Self {
            // SAFETY: the handle is open, non-null and owned by no one else.
            handle: unsafe { OwnedHandle::from_raw_handle(handle) },
        })
    }

    /// Waits up to `timeout` for the process to end.
    pub(super) fn wait(&self, timeout: Duration) -> Waited {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: `handle` is an open process handle owned by `self` for the
        // whole call, and the wait takes no pointer.
        let result = unsafe { WaitForSingleObject(self.handle.as_raw_handle(), millis) };
        match result {
            WAIT_OBJECT_0 => Waited::Ended,
            WAIT_TIMEOUT => Waited::Running,
            _ => Waited::Failed,
        }
    }
}
