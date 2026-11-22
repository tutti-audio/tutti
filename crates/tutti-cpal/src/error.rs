//! Errors from the device layer.

use tutti_engine::{BackendErrorKind, StartError};

/// A failure opening, configuring, or running an audio device.
///
/// Every variant is a device concern; failures from other subsystems (synth,
/// plugin, export) are the host's to aggregate. Errors reported by a stream
/// that is already running do not come back as a `DeviceError`: CPAL delivers them
/// to [`StreamFaults`](tutti_engine::StreamFaults).
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum DeviceError {
    /// No default output stream config was available from the host.
    #[error("Audio device not available")]
    DeviceNotAvailable(#[source] cpal::Error),

    /// The CPAL stream could not be built with the requested config.
    #[error("Failed to build audio stream")]
    BuildStream(#[source] cpal::Error),

    /// The CPAL stream could not be started.
    #[error("Failed to play audio stream")]
    PlayStream(#[source] cpal::Error),

    /// Enumerating audio devices via CPAL failed.
    #[error("Failed to enumerate devices")]
    DevicesError(#[source] cpal::Error),

    /// The requested stream configuration was rejected as invalid.
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    /// The selected audio device could not be used (missing, unsupported, etc.).
    #[error("Invalid device: {0}")]
    InvalidDevice(String),

    /// The requested audio host is not reachable in this build or on this
    /// machine — the feature is off, the platform has no such host, or the
    /// server is not running.
    ///
    /// A runtime error rather than a compile error: [`AudioHost`](crate::AudioHost)
    /// has every variant on every platform, and
    /// [`available_hosts`](crate::available_hosts) lists the reachable ones.
    #[error("audio host {host} unavailable: {reason}")]
    HostUnavailable {
        /// The host that was asked for.
        host: crate::host::AudioHost,
        /// Why it cannot be opened, as the backend or this crate put it.
        reason: String,
    },

    /// A capture device cannot run at the graph's sample rate.
    ///
    /// Returned by `MicIn::open` (feature `capture`): the mic is rendered into
    /// the graph with no resampling, so it must run at the graph's rate.
    #[error("input device {device_name:?} runs at {device} Hz; the graph runs at {graph} Hz")]
    SampleRateMismatch {
        /// The input device's name, as the OS reports it.
        device_name: String,
        /// The rate of the device's default input config.
        device: tutti_units::SampleRate,
        /// The rate that was asked for.
        graph: tutti_units::SampleRate,
    },

    /// A plain [`TuttiDriver::restart`](crate::TuttiDriver::restart) found
    /// the output device at a different rate than the graph was built at.
    ///
    /// Everything time-denominated in the graph — every oscillator, every
    /// delay in samples, the beat clock — would run at the graph's rate on
    /// the new device, off pitch and off tempo with no error anywhere, so the
    /// stream is left stopped instead. Restart with
    /// [`TuttiDriver::restart_with`](crate::TuttiDriver::restart_with) and
    /// re-rate the graph in its hook.
    #[error(
        "the output device now runs at {device} Hz, the graph at {graph} Hz; \
         restart with `restart_with` and re-rate the graph"
    )]
    RateChanged {
        /// The rate the output device now reports.
        device: tutti_units::SampleRate,
        /// The rate the graph runs at ([`TuttiDriver::graph_rate`](crate::TuttiDriver::graph_rate)).
        graph: tutti_units::SampleRate,
    },

    /// A capture device does not offer the sample format the app chose at the
    /// graph's rate ([`MicConfig::sample_format`](crate::MicConfig)).
    ///
    /// Returned by `MicIn::open` (feature `capture`): the format is never
    /// swapped for another.
    #[error("input device {device_name:?} does not offer {format:?} samples at {rate} Hz")]
    InputFormatUnavailable {
        /// The input device's name, as the OS reports it.
        device_name: String,
        /// The format that was asked for.
        format: tutti_buffers::SampleFormat,
        /// The graph's rate the device was asked to run at.
        rate: tutti_units::SampleRate,
    },

    /// An output device does not offer the sample format the app chose
    /// ([`OutputConfig::sample_format`](crate::OutputConfig::sample_format)) at
    /// its default rate and width.
    ///
    /// The format is never swapped for another.
    #[error("output device {device_name:?} does not offer {format:?} samples")]
    OutputFormatUnavailable {
        /// The output device's name, as the OS reports it.
        device_name: String,
        /// The format that was asked for.
        format: tutti_buffers::SampleFormat,
    },

    /// The output device does not offer the callback size asked for
    /// ([`OutputConfig::quantum`](crate::OutputConfig::quantum)): it is past the
    /// range the device reports. The size is never clamped to the range.
    #[error(
        "output device {device_name:?} offers callbacks of {min} to {max} frames, not {}",
        .quantum.get()
    )]
    QuantumUnavailable {
        /// The output device's name, as the OS reports it.
        device_name: String,
        /// The callback size that was asked for.
        quantum: tutti_engine::OutputQuantum,
        /// The fewest frames the device offers.
        min: u32,
        /// The most frames the device offers.
        max: u32,
    },

    /// The inputs asked of a capture device cannot be selected: none were
    /// named, or one is past the device's last.
    ///
    /// Returned by `MicIn::open` (feature `capture`).
    #[error("cannot select those inputs: {0}")]
    Inputs(#[from] tutti_buffers::FoldError),

    /// [`AudioEngine::start`](crate::AudioEngine::start) or
    /// [`start_with`](crate::AudioEngine::start_with) was called while a
    /// stream is running ([`StartError::AlreadyRunning`]).
    ///
    /// The new state is not adopted and the running stream is untouched; stop
    /// the engine first to replace it.
    #[error("the audio stream is already running; stop it before starting another")]
    AlreadyRunning,

    /// A [`DeviceListing`](crate::DeviceListing)'s task ended without an
    /// answer (it panicked), so there is none to wait for.
    #[error("the device listing task ended without an answer")]
    ListingEnded,

    /// I/O failure while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl DeviceError {
    /// How the backend's own error reads, for a variant that carries one
    /// ([`DeviceNotAvailable`](Self::DeviceNotAvailable),
    /// [`BuildStream`](Self::BuildStream), [`PlayStream`](Self::PlayStream),
    /// [`DevicesError`](Self::DevicesError)): the same reading a running stream's
    /// faults carry.
    ///
    /// The three-way reading ([`DeviceGone`](BackendErrorKind::DeviceGone),
    /// [`Survivable`](BackendErrorKind::Survivable),
    /// [`Other`](BackendErrorKind::Other)) is the whole contract: it is what an
    /// app acts on (restart, keep playing, report), and the backend's own error
    /// kinds are not part of this crate's API, so a backend upgrade never
    /// changes what an app matches.
    #[must_use]
    pub fn backend_kind(&self) -> Option<BackendErrorKind> {
        match self {
            Self::DeviceNotAvailable(e)
            | Self::BuildStream(e)
            | Self::PlayStream(e)
            | Self::DevicesError(e) => Some(crate::edge::backend_kind(e)),
            _ => None,
        }
    }
}

impl From<std::convert::Infallible> for DeviceError {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}

impl<E: std::error::Error + Into<DeviceError> + 'static> From<StartError<E>> for DeviceError {
    fn from(err: StartError<E>) -> Self {
        match err {
            StartError::AlreadyRunning => Self::AlreadyRunning,
            StartError::Driver(e) => e.into(),
            // `StartError` is non-exhaustive; a reason added there needs a row here.
            other => Self::InvalidConfig(other.to_string()),
        }
    }
}

/// Convenience alias for `Result<T, `[`enum@DeviceError`]`>`.
pub type Result<T> = core::result::Result<T, DeviceError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Each variant that carries a backend error reads as the engine's
    /// three-way kind: a device gone is `DeviceGone`, a reroute or a denied
    /// real-time priority is `Survivable`, anything else is `Other`; a variant
    /// with no backend error reads `None`.
    #[test]
    fn backend_kind_reads_the_four_backend_variants() {
        let of = |kind| cpal::Error::new(kind);
        let gone = || of(cpal::ErrorKind::DeviceNotAvailable);
        assert_eq!(
            DeviceError::DeviceNotAvailable(gone()).backend_kind(),
            Some(BackendErrorKind::DeviceGone)
        );
        assert_eq!(
            DeviceError::BuildStream(of(cpal::ErrorKind::DeviceChanged)).backend_kind(),
            Some(BackendErrorKind::Survivable)
        );
        assert_eq!(
            DeviceError::PlayStream(of(cpal::ErrorKind::RealtimeDenied)).backend_kind(),
            Some(BackendErrorKind::Survivable)
        );
        assert_eq!(
            DeviceError::DevicesError(of(cpal::ErrorKind::DeviceBusy)).backend_kind(),
            Some(BackendErrorKind::Other)
        );
        assert_eq!(DeviceError::AlreadyRunning.backend_kind(), None);
    }
}
