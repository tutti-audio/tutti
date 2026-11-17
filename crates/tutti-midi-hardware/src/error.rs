//! The crate's [`Error`](enum@Error) and [`Result`].
//!
//! One enum for every OS edge. [`Error::Os`] carries the platform's own FFI
//! error (the call that failed and the status it returned), because that pair
//! is what makes a driver failure diagnosable — a bare string loses the code a
//! caller may want to match on.

use thiserror::Error;

/// What a lookup did not find.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum NotFound {
    /// No device is present under this id: what a stale id resolves to.
    #[error("no MIDI device {0}")]
    Device(tutti_midi_device::DeviceId),

    /// No endpoint answers to this id: a handle the OS has since retired, or
    /// one this platform's backend never minted.
    #[error("no MIDI endpoint {}", .0.raw())]
    Endpoint(tutti_midi_device::EndpointId),
}

/// What can go wrong opening or driving an OS MIDI endpoint.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// A filesystem or descriptor error surfaced from `std::io`.
    #[error("IO error")]
    Io(#[from] std::io::Error),

    /// Opening, configuring or connecting a port failed, with the backend's own
    /// description and, when there was one, the error under it
    /// ([`source`](std::error::Error::source)). Raised directly by the CoreMIDI
    /// and ALSA backends; made with [`Error::port`] and [`Error::port_from`].
    #[error("MIDI port error: {what}")]
    MidiPort {
        /// What failed, in words.
        what: String,
        /// The error under it, if the failure came from one.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// What was asked for is not there: a stale or foreign id.
    #[error(transparent)]
    NotFound(#[from] NotFound),

    /// The device is present but does not go the way asked: an output opened
    /// as an input, or the reverse.
    #[error("MIDI device {id} is {direction:?}")]
    WrongDirection {
        /// The device.
        id: tutti_midi_device::DeviceId,
        /// Which way it does go.
        direction: tutti_midi_device::PortDirection,
    },

    /// No present device has a name containing this text. Carries the names it
    /// searched (the devices of the direction asked for), so a typo shows what
    /// was there.
    #[error("no MIDI device named like '{name}' (searched: {})", .searched.join(", "))]
    NoDeviceNamed {
        /// The text that matched nothing.
        name: String,
        /// The names of the devices it was matched against; empty when none
        /// was present.
        searched: Vec<String>,
    },

    /// The named device could not be driven, with the backend's own
    /// description and, when there was one, the error under it
    /// ([`source`](std::error::Error::source)). Made with [`Error::device`]
    /// and [`Error::device_from`].
    #[error("MIDI device error: {what}")]
    MidiDevice {
        /// What failed, in words.
        what: String,
        /// The error under it, if the failure came from one.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// A call into the platform's MIDI API failed: ALSA's, CoreMIDI's or
    /// winmm's own error, with the call that failed and the status it
    /// returned. Where the platform has no backend this is
    /// [`Infallible`](std::convert::Infallible).
    #[error(transparent)]
    Os(#[from] crate::backend::OsError),

    /// An output's pool has fewer free cells than a message of several packets
    /// needs, so none of it was sent.
    #[error("MIDI output pool has {free} free cells, and the message needs {needed}")]
    OutputPoolFull {
        /// Free cells now.
        free: usize,
        /// Cells the message needs.
        needed: usize,
    },

    /// A backend seeded its device list twice, or after reporting a change.
    #[error(transparent)]
    Seed(#[from] crate::SeedError),

    /// No MIDI backend exists for this build.
    ///
    /// Distinct from "no devices found", which is an empty list and not an
    /// error. This says the *platform* has no backend compiled in (a Linux built
    /// against an alsa-lib older than 1.2.10), or that its API cannot do what
    /// was asked (Windows Multimedia has no virtual ports). A caller that
    /// shows the user "no MIDI devices" for this case is hiding a build fact
    /// behind a runtime one.
    #[error("MIDI backend unavailable: {0}")]
    Unsupported(&'static str),

    /// A MIDI 2.0 message sent into a MIDI 1.0 virtual endpoint has no MIDI 1.0
    /// message to become; nothing was sent.
    #[error(transparent)]
    NoMidi1Form(#[from] tutti_midi_protocol::NoMidi1Form),

    /// A virtual endpoint declared Function Blocks, and the platform cannot
    /// publish them: its endpoints carry a name and a protocol only. The same
    /// spec without blocks opens.
    #[error("this platform cannot publish Function Blocks on a virtual endpoint")]
    VirtualBlocksUnsupported,
    // NOTE: there is deliberately no `File(tutti_midi_file::Error)` variant.
    // A parse failure is not a port failure — a consumer that reads files owns
    // that error itself (`tutti_midi_file::Error`, which this crate does not
    // depend on).
}

/// An error and the errors under it, as one line: `outer: inner: innermost`.
///
/// A failed call's error names the call and keeps the OS error as its source
/// (alsa-ffi's `Call` does), so a log line prints the chain to show both.
pub(crate) fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(under) = source {
        text.push_str(": ");
        text.push_str(&under.to_string());
        source = under.source();
    }
    text
}

impl Error {
    /// A [`MidiPort`](Self::MidiPort) error with no error under it.
    #[must_use]
    pub fn port(what: impl Into<String>) -> Self {
        Self::MidiPort {
            what: what.into(),
            source: None,
        }
    }

    /// A [`MidiPort`](Self::MidiPort) error caused by `source`.
    #[must_use]
    pub fn port_from(
        what: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::MidiPort {
            what: what.into(),
            source: Some(Box::new(source)),
        }
    }

    /// A [`MidiDevice`](Self::MidiDevice) error with no error under it.
    #[must_use]
    pub fn device(what: impl Into<String>) -> Self {
        Self::MidiDevice {
            what: what.into(),
            source: None,
        }
    }

    /// A [`MidiDevice`](Self::MidiDevice) error caused by `source`.
    #[must_use]
    pub fn device_from(
        what: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::MidiDevice {
            what: what.into(),
            source: Some(Box::new(source)),
        }
    }
}

/// The crate's result type: [`Error`](enum@Error) on the failure side.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
