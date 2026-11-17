//! A file's modification time as the database stamps it.

use std::time::SystemTime;

/// A file's last modification, at or after the Unix epoch.
///
/// The record stores a time as seconds and nanoseconds since the epoch, so a
/// time before it cannot be held; it has no `Mtime`, and a file with such a
/// time is stamped with none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "SystemTime", into = "SystemTime"))]
pub struct Mtime(SystemTime);

/// The time lies before the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the time lies before the unix epoch")]
pub struct BeforeEpoch;

impl Mtime {
    /// `time`, or `None` if it lies before the Unix epoch.
    #[must_use]
    pub fn new(time: SystemTime) -> Option<Self> {
        (time >= SystemTime::UNIX_EPOCH).then_some(Self(time))
    }

    /// The time.
    #[must_use]
    pub fn get(self) -> SystemTime {
        self.0
    }
}

impl TryFrom<SystemTime> for Mtime {
    type Error = BeforeEpoch;

    fn try_from(time: SystemTime) -> Result<Self, BeforeEpoch> {
        Self::new(time).ok_or(BeforeEpoch)
    }
}

impl From<Mtime> for SystemTime {
    fn from(mtime: Mtime) -> Self {
        mtime.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A time before the epoch has no `Mtime`, by `new` and by `try_from`; the
    /// epoch itself and later times do.
    #[test]
    fn a_time_before_the_epoch_is_refused() {
        let before = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(Mtime::new(before), None);
        assert_eq!(Mtime::try_from(before), Err(BeforeEpoch));
        let epoch = SystemTime::UNIX_EPOCH;
        assert_eq!(Mtime::new(epoch).map(Mtime::get), Some(epoch));
        let later = epoch + Duration::new(5, 7);
        assert_eq!(SystemTime::from(Mtime::try_from(later).unwrap()), later);
    }
}
