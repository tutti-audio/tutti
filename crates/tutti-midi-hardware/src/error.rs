//! The crate's [`MidiHardwareError`] and [`Result`].
//!
//! One enum for every OS edge. [`MidiHardwareError::Os`] carries the platform's own FFI
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

/// A thread this crate starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MidiThread {
    /// The device watch: a backend's, or the fake's.
    DeviceWatch,
    /// An input's pump.
    MidiIn,
    /// An output's sender.
    MidiOut,
}

impl MidiThread {
    /// The name the thread runs under.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DeviceWatch => "tutti-devwatch",
            Self::MidiIn => "tutti-midi-in",
            Self::MidiOut => "tutti-midi-out",
        }
    }
}

/// What can go wrong opening or driving an OS MIDI endpoint.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum MidiHardwareError {
    /// A thread could not start: the OS refused it.
    #[error("cannot start the {} thread", .thread.name())]
    Spawn {
        /// The thread.
        thread: MidiThread,
        /// The OS's refusal.
        #[source]
        source: std::io::Error,
    },

    /// A system call outside the platform's MIDI API failed.
    #[error("cannot {step}")]
    Io {
        /// What the call was for, as a verb phrase.
        step: &'static str,
        /// The OS's error.
        #[source]
        source: std::io::Error,
    },

    /// The sequencer's descriptor failed while the watch waited on it.
    #[error("the sequencer descriptor failed")]
    DescriptorBroken,

    /// A thread ended before it reported that it was ready.
    #[error("the {} thread ended before it was ready", .thread.name())]
    ThreadEnded {
        /// The thread.
        thread: MidiThread,
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

    /// A name matches more than one device, so none is picked: the caller names
    /// one by its [`DeviceId`](tutti_midi_device::DeviceId), or by more of its
    /// name. Carries the names it matched.
    #[error("the name '{name}' matches several MIDI devices: {}", .matches.join(", "))]
    AmbiguousDeviceName {
        /// The text that matched more than one device.
        name: String,
        /// The names of the devices it matched, in the order searched.
        matches: Vec<String>,
    },

    /// The device has no open input: it was never connected, or was closed.
    #[error("MIDI device {0} has no open input")]
    NotConnected(tutti_midi_device::DeviceId),

    /// Another connection or application holds the endpoint, and the OS lets
    /// only one.
    #[error("MIDI endpoint {} is in use", .0.raw())]
    InUse(tutti_midi_device::EndpointId),

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
    #[error("a backend could not seed its device list")]
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
    NoMidi1Form(#[from] tutti_ump::NoMidi1Form),

    /// A virtual endpoint declared Function Blocks, and the platform cannot
    /// publish them: its endpoints carry a name and a protocol only (macOS before
    /// 15, which has no `MIDIUMPMutableEndpoint`). The same spec without blocks
    /// opens.
    #[error("this platform cannot publish Function Blocks on a virtual endpoint")]
    VirtualBlocksUnsupported,

    /// A send on a virtual endpoint that was dropped: its
    /// [`VirtualSender`](crate::VirtualSender) outlived it.
    #[error("the virtual endpoint was dropped")]
    EndpointGone,
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

/// The crate's result type: [`MidiHardwareError`] on the failure side.
pub type Result<T> = std::result::Result<T, MidiHardwareError>;

#[cfg(test)]
mod tests;
