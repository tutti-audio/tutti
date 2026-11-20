//! The OS side of device watching: a thread that enumerates the host and
//! publishes what it finds.
//!
//! cpal 0.15 has no change notification on any host, and enumeration can
//! block (ALSA probes cards, WASAPI enumerates endpoints). So it runs on a
//! thread of its own, never the audio thread and never inside
//! `Session::update`, which only takes the newest result
//! ([`SnapshotSource::poll`]).

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;
use tutti_platform::StopToken;

use crate::error::{DeviceError, Result};
use crate::host::{AudioHost, DeviceHost};
use crate::roster::{Snapshot, SnapshotSource};

/// The shortest interval a poller keeps: enumeration can open every device.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A thread enumerating one host every `interval` (at least
/// [`MIN_POLL_INTERVAL`]).
///
/// Only a snapshot that differs from the one before it is published. The
/// thread opens its own [`DeviceHost`] and ends once the poller drops (after
/// the enumeration it may be in the middle of; dropping never waits for it).
#[derive(Debug)]
pub struct DevicePoller {
    snapshots: Receiver<Snapshot>,
    /// Stopped when the poller drops: the thread's wait ends, and so does it.
    stop: StopToken,
}

impl Drop for DevicePoller {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

impl DevicePoller {
    /// Starts polling `host` every `interval`, the first look at once.
    ///
    /// # Errors
    /// [`DeviceError::HostUnavailable`] when the host cannot be opened, or
    /// [`DeviceError::InvalidConfig`] when the thread cannot be spawned.
    pub fn spawn(host: AudioHost, interval: Duration) -> Result<Self> {
        let interval = interval.max(MIN_POLL_INTERVAL);
        // Opened here to report an unreachable host to the caller; the thread
        // opens its own, so no host crosses a thread.
        DeviceHost::open(host)?;
        let (tx, snapshots) = mpsc::channel();
        // Detached: dropping the poller stops it without waiting on an
        // enumeration in progress.
        let thread = tutti_platform::spawn_named("tutti-device-poll", None, move |stop| {
            let Ok(host) = DeviceHost::open(host) else {
                return;
            };
            let mut last: Option<Snapshot> = None;
            loop {
                if let Ok(snap) = host.snapshot()
                    && last.as_ref() != Some(&snap)
                {
                    last = Some(snap.clone());
                    if tx.send(snap).is_err() {
                        return;
                    }
                }
                if stop.wait(interval) {
                    return;
                }
            }
        })
        .map_err(|e| DeviceError::InvalidConfig(format!("cannot start the device poller: {e}")))?;
        let stop = thread.token();
        thread.detach();
        Ok(Self { snapshots, stop })
    }
}

impl SnapshotSource for DevicePoller {
    fn poll(&mut self) -> Option<Snapshot> {
        self.snapshots.try_iter().last()
    }
}
