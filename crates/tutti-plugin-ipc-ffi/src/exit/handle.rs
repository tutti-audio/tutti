//! Windows: a process handle is signalled when the process ends.

use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::process::Child;
use std::time::Duration;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
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

    pub(super) fn wait(&self, timeout: Duration) -> bool {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: `handle` is an open process handle owned by `self` for the
        // whole call, and the wait takes no pointer.
        let result = unsafe { WaitForSingleObject(self.handle.as_raw_handle(), millis) };
        // Only a timeout says "still running"; a failure to ask is "may have
        // ended" and the owner's `try_wait` decides.
        result != WAIT_TIMEOUT
    }
}
