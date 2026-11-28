//! [`AudioHost`]: which platform host devices open through.

/// Which platform audio host to open devices through.
///
/// Every variant exists on every platform, so a configuration struct holding
/// an `AudioHost` has the same shape on Linux, macOS and Windows (cpal's own
/// `HostId` is generated per target). Asking for a host this build cannot
/// reach fails at runtime with [`DeviceError::HostUnavailable`](crate::DeviceError::HostUnavailable); use
/// [`available_hosts`](crate::available_hosts) to list the ones that work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioHost {
    /// Whatever cpal calls the platform default: ALSA, WASAPI, CoreAudio.
    Default,
    /// JACK, on Linux and the BSDs.
    ///
    /// Needs this crate's `jack` feature. Elsewhere, or without the feature,
    /// opening it returns [`DeviceError::HostUnavailable`](crate::DeviceError::HostUnavailable).
    Jack,
}

impl std::fmt::Display for AudioHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioHost::Default => f.write_str("default"),
            AudioHost::Jack => f.write_str("JACK"),
        }
    }
}
