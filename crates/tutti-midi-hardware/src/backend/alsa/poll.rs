//! How many read errors in a row a reader puts up with.
//!
//! The input pump and the device watch both read a non-blocking client until it
//! is empty, then wait on `tutti_alsa_ffi::Poller`.

use crate::InputFault;
use crate::fault::Errno;
use tutti_alsa_ffi::Wait;

/// Read errors in a row a reader puts up with before it stops.
pub(super) const MAX_DRAIN_ERRORS: usize = 8;

/// Read errors in a row, which end a reader at [`MAX_DRAIN_ERRORS`]. A read that
/// succeeds starts the count again, so scattered overruns in a long burst do
/// not end the reader with events unread.
#[derive(Debug, Default)]
pub(super) struct ErrorRun(usize);

impl ErrorRun {
    /// Counts an error; true when the run has reached the limit.
    pub(super) fn failed(&mut self) -> bool {
        self.0 += 1;
        self.0 >= MAX_DRAIN_ERRORS
    }

    /// A read succeeded.
    pub(super) fn read(&mut self) {
        self.0 = 0;
    }
}

/// The fault a wait that did not ready or stop the pump ends it with: `None`
/// for [`Wait::Ready`] and [`Wait::Stopped`].
pub(super) fn wait_fault(waited: &std::io::Result<Wait>) -> Option<InputFault> {
    match waited {
        Ok(Wait::Ready | Wait::Stopped) => None,
        Ok(Wait::Broken) => Some(InputFault::PortBroken),
        Err(e) => Some(InputFault::WaitFailed(
            e.raw_os_error().and_then(Errno::clamped),
        )),
    }
}

/// The fault a reader ends with when its reads keep failing, the last with
/// `errno`.
pub(super) fn read_fault(errno: Option<i32>) -> InputFault {
    InputFault::ReadFailed(errno.and_then(Errno::clamped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// **A wait that readied or stopped the pump is no fault; a broken port and
    /// a failed call are, each for its own cause**, with the call's errno.
    #[test]
    fn a_wait_is_a_fault_for_what_ended_it() {
        assert_eq!(wait_fault(&Ok(Wait::Ready)), None);
        assert_eq!(wait_fault(&Ok(Wait::Stopped)), None);
        assert_eq!(wait_fault(&Ok(Wait::Broken)), Some(InputFault::PortBroken));
        assert_eq!(
            wait_fault(&Err(io::Error::from_raw_os_error(4))),
            Some(InputFault::WaitFailed(Errno::new(4)))
        );
        assert_eq!(
            wait_fault(&Err(io::Error::other("no number"))),
            Some(InputFault::WaitFailed(None))
        );
    }

    /// **A read fault carries the last error's number**, none when it had
    /// none, and saturates one past 24 bits.
    #[test]
    fn a_read_fault_carries_the_errno() {
        assert_eq!(read_fault(Some(5)), InputFault::ReadFailed(Errno::new(5)));
        assert_eq!(read_fault(None), InputFault::ReadFailed(None));
        assert_eq!(
            read_fault(Some(i32::MAX)),
            InputFault::ReadFailed(Errno::new(0x00FF_FFFF))
        );
    }
}
