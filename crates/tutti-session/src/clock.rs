//! [`Clock`]: what runs a session's blocks, and so the session's type.
//!
//! Every session differs at its root by one thing: the app steps an
//! [`Offline`] session (`Session::render`), a sound card's callback steps a
//! `DeviceClock` session, an OS timer thread steps a [`SystemClock`] one. The
//! rest follows from it: a sound card's audio can be played and captured only
//! on the clock of that sound card (an inlet whose rate is not the graph's is
//! refused, never resampled), so the device's calls belong to the session on
//! the device clock, and a session on the system clock has no audio in or out.
//! A call a clock cannot serve does not exist on its session.

/// A session on the offline clock, which the app steps with `render`.
pub type OfflineSession = crate::Session<Offline>;

/// A session on the system clock, which an OS timer thread steps: no audio in
/// or out. It is `Send`, so an app may build it on one thread and run it on
/// another:
///
/// ```
/// fn is_send<T: Send>() {}
/// is_send::<tutti_session::SystemSession>();
/// ```
///
/// A discarded build is a warning (an error under `deny(unused_must_use)`):
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// fn build() -> tutti_session::SystemSession { unimplemented!() }
/// fn f() {
///     build();
/// }
/// ```
pub type SystemSession = crate::Session<SystemClock>;

use tutti_engine::{Engine, OutputStream};

use crate::BlockConfig;

mod sealed {
    pub trait Sealed {}
}

/// What runs a session's blocks: [`Offline`], `DeviceClock` (feature
/// `device`) or [`SystemClock`]. Sealed.
///
/// A clock is a zero-sized marker; its data is [`Config`](Self::Config) while
/// the session is being built and [`Running`](Self::Running) once it is.
pub trait Clock: sealed::Sealed + Sized + 'static {
    /// What a [`SessionBuilder`](crate::SessionBuilder) on this clock holds:
    /// the block sizes for a clock the session sets, the device selector for
    /// the sound card, which sets them itself.
    type Config: std::fmt::Debug;
    /// What the built session holds to run its blocks: the engine the app
    /// pulls, the device, or the timer's stream.
    type Running: std::fmt::Debug;

    /// The stream that renders this clock's blocks, if one does: its fault
    /// record is the session's ([`RealTime`]).
    #[doc(hidden)]
    fn stream(running: &Self::Running) -> Option<&OutputStream>;
}

/// A clock that runs blocks on its own: `DeviceClock` and [`SystemClock`].
/// Sealed.
///
/// What both share is on `Session<C: RealTime>`: the stream's
/// [`faults`](crate::Session::faults), which `Session::update` reports as
/// [`SessionEvent::Fault`](crate::SessionEvent::Fault).
pub trait RealTime: Clock {
    /// The stream that renders this clock's blocks.
    #[doc(hidden)]
    fn output_stream(running: &Self::Running) -> &OutputStream;
}

/// The clock of a session the app steps: the host pulls each block with
/// [`Session::render`](crate::Session::render), from a game's or a plugin's own
/// real-time callback or from a loop faster than real time.
#[derive(Clone, Copy, Debug, Default)]
pub struct Offline;

/// The clock of a session an OS timer thread steps
/// ([`SessionBuilder::system_clock`](crate::SessionBuilder::system_clock)): no
/// sound card, for an app that plays MIDI.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

/// The clock of a session a sound card's callback steps
/// ([`SessionBuilder::device`](crate::SessionBuilder::device)): the device sets
/// the rate, the width and the callback size.
#[cfg(feature = "device")]
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceClock;

impl sealed::Sealed for Offline {}
impl sealed::Sealed for SystemClock {}
#[cfg(feature = "device")]
impl sealed::Sealed for DeviceClock {}

/// What an offline session holds: the engine its host pulls blocks from, or
/// none for a detached session (`SessionBuilder::build_detached`, behind
/// `testing`), whose graph the caller renders by hand.
#[derive(Debug)]
pub struct OfflineEngine {
    pub(crate) engine: Option<Engine>,
}

/// What a session on the system clock holds: the stream the timer thread runs
/// the callback on. Dropping the session stops the thread.
#[derive(Debug)]
pub struct SystemEngine {
    pub(crate) stream: OutputStream,
}

impl Clock for Offline {
    type Config = BlockConfig;
    type Running = OfflineEngine;

    fn stream(_: &OfflineEngine) -> Option<&OutputStream> {
        None
    }
}

impl Clock for SystemClock {
    type Config = BlockConfig;
    type Running = SystemEngine;

    fn stream(running: &SystemEngine) -> Option<&OutputStream> {
        Some(<Self as RealTime>::output_stream(running))
    }
}

impl RealTime for SystemClock {
    fn output_stream(running: &SystemEngine) -> &OutputStream {
        &running.stream
    }
}

#[cfg(feature = "device")]
impl Clock for DeviceClock {
    type Config = tutti_cpal::DeviceSelector;
    type Running = crate::DeviceHandle;

    fn stream(running: &crate::DeviceHandle) -> Option<&OutputStream> {
        Some(<Self as RealTime>::output_stream(running))
    }
}

#[cfg(feature = "device")]
impl RealTime for DeviceClock {
    fn output_stream(running: &crate::DeviceHandle) -> &OutputStream {
        running.device().output_stream()
    }
}
