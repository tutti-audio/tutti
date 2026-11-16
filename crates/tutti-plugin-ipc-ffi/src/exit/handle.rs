//! Windows: a process handle is signalled when the process ends.

use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::process::Child;
use std::time::Duration;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

#[derive(Debug)]
pub(super) struct Imp {
    /// A duplicate of the child's handle, so the wait outlives no close of
    /// the original; `None` when the duplicate failed and the wait sleeps.
    handle: Option<OwnedHandle>,
}

impl Imp {
    pub(super) fn of(child: &Child) -> Self {
        Self {
            handle: child.as_handle().try_clone_to_owned().ok(),
        }
    }

    pub(super) fn wait(&self, timeout: Duration) -> bool {
        let Some(handle) = &self.handle else {
            std::thread::sleep(timeout.min(super::NAP));
            return true;
        };
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: `handle` is an open process handle owned by `self` for the
        // whole call, and the wait takes no pointer.
        let result = unsafe { WaitForSingleObject(handle.as_raw_handle(), millis) };
        // Only a timeout says "still running"; a failure to ask is "may have
        // ended" and the owner's `try_wait` decides.
        result != WAIT_TIMEOUT
    }
}
