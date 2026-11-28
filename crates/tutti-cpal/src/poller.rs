//! The OS side of device watching: a value whose blocking step looks at the host.
//!
//! cpal 0.15 has no change notification on any host, and enumeration can
//! block (ALSA probes cards, WASAPI enumerates endpoints). So a look is a
//! blocking step the caller runs off its queue: the session's loop runs it on
//! the executor's blocking pool, never on the audio thread and never inside
//! `Session::update`.

use std::time::Duration;

use crate::audio_host::AudioHost;
use crate::error::Result;
use crate::host::DeviceHost;
use crate::roster::Snapshot;

tutti_units::ranged! {
    /// The time between two looks at a host: at least [`MIN`](PollInterval::MIN),
    /// since enumeration can open every device.
    PollInterval(Duration), PollIntervalTooShort, "at least 250 ms",
    |raw| raw.as_millis() >= 250
}

impl PollInterval {
    /// The shortest interval, 250 ms.
    pub const MIN: Self = Self(Duration::from_millis(250));
}

/// One host looked at every `interval`.
///
/// It holds no thread or task: [`look`](Self::look) is the blocking step and
/// the caller owns the cadence ([`interval`](Self::interval)). Only a
/// snapshot that differs from the one before it is returned.
#[derive(Debug)]
pub struct DevicePoller {
    host: AudioHost,
    interval: PollInterval,
    opened: Option<DeviceHost>,
    last: Option<Snapshot>,
}

impl DevicePoller {
    /// A poller of `host` that looks every `interval`. Nothing is opened until
    /// the first [`look`](Self::look).
    #[must_use]
    pub fn new(host: AudioHost, interval: PollInterval) -> Self {
        Self {
            host,
            interval,
            opened: None,
            last: None,
        }
    }

    /// The time between looks, as given to [`new`](Self::new).
    #[must_use]
    pub fn interval(&self) -> PollInterval {
        self.interval
    }

    /// Looks at the host: the snapshot when it differs from the last one
    /// returned (the first look always), else `None`. The first call opens the
    /// host, and a failed open is tried again by the next call.
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
    pub fn look(&mut self) -> Result<Option<Snapshot>> {
        let host = match &self.opened {
            Some(host) => host,
            None => self.opened.insert(DeviceHost::open(self.host)?),
        };
        let snapshot = host.snapshot()?;
        if self.last.as_ref() == Some(&snapshot) {
            return Ok(None);
        }
        self.last = Some(snapshot.clone());
        Ok(Some(snapshot))
    }
}

const _: () = {
    const fn is_send<T: Send>() {}
    is_send::<DevicePoller>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// **An interval is at least 250 ms.** One a millisecond short is refused,
    /// never raised to the minimum, and the poller reads back what it was given.
    #[test]
    fn the_interval_is_at_least_the_minimum() {
        tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
            new: PollInterval::new,
            get: PollInterval::get,
            of: Some(PollInterval::of),
            valid: &[Duration::from_millis(250), Duration::from_secs(3)],
            invalid: &[Duration::ZERO, Duration::from_millis(249)],
        });
        assert_eq!(PollInterval::MIN.get(), Duration::from_millis(250));
        let given = PollInterval::of(Duration::from_secs(3));
        assert_eq!(
            DevicePoller::new(AudioHost::Default, given).interval(),
            given
        );
    }
}
