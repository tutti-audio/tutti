//! One enumeration of the output devices, a value with one blocking step.
//!
//! Enumeration can block for good (ALSA opens every PCM, and one inside a
//! sound server's plugin can wait on that server's thread forever), so it
//! never runs on the thread that builds a frame. [`DeviceListing`] is the
//! one-shot counterpart of [`DevicePoller`](crate::DevicePoller): it holds no
//! thread or task, and [`list`](DeviceListing::list) is the step a caller
//! runs off its queue (the session runs it on the executor's blocking pool).

use std::fmt;

use crate::error::Result;
use crate::host::{AudioHost, DeviceHost};
use crate::roster::{DeviceDirection, DeviceId};

type ListFn = Box<dyn FnOnce() -> Result<Vec<DeviceId>> + Send>;

/// A description of one enumeration of a host's output devices.
///
/// Building it touches no host; [`list`](Self::list) opens one.
pub struct DeviceListing {
    list: ListFn,
}

impl fmt::Debug for DeviceListing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceListing").finish_non_exhaustive()
    }
}

impl DeviceListing {
    /// A listing of `host`'s output devices.
    #[must_use]
    pub fn new(host: AudioHost) -> Self {
        Self::from_fn(move || {
            Ok(DeviceHost::open(host)?
                .snapshot()?
                .devices(DeviceDirection::Output)
                .to_vec())
        })
    }

    /// A listing that runs `list`: the fake a test enumerates through.
    #[must_use]
    pub fn from_fn(list: impl FnOnce() -> Result<Vec<DeviceId>> + Send + 'static) -> Self {
        Self {
            list: Box::new(list),
        }
    }

    /// Opens the host and returns its output devices.
    ///
    /// **Blocks** for one enumeration, which can wait on a sound server for
    /// good: run it off any queue thread (the session runs it on the
    /// executor's blocking pool).
    ///
    /// # Errors
    /// [`DeviceError::HostUnavailable`](crate::DeviceError::HostUnavailable)
    /// when the host cannot be opened,
    /// [`DeviceError::DevicesError`](crate::DeviceError::DevicesError) when it
    /// cannot enumerate.
    pub fn list(self) -> Result<Vec<DeviceId>> {
        (self.list)()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake's answer is what `list` returns.
    #[test]
    fn from_fn_lists_what_the_fake_answers() {
        let listing = DeviceListing::from_fn(|| Ok(DeviceId::list(["A", "B"])));
        assert_eq!(listing.list().unwrap(), DeviceId::list(["A", "B"]));
    }

    /// A fake's failure is returned as it is.
    #[test]
    fn from_fn_returns_the_fakes_failure() {
        let listing =
            DeviceListing::from_fn(|| Err(crate::DeviceError::InvalidDevice("no card".into())));
        assert!(matches!(
            listing.list(),
            Err(crate::DeviceError::InvalidDevice(_))
        ));
    }
}
