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
pub type OfflineSession<K = &'static str, F = f32> = crate::Session<K, Offline<F>>;

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
pub type SystemSession<K = &'static str> = crate::Session<K, SystemClock>;

use std::marker::PhantomData;

use tutti_buffers::Float;
use tutti_engine::{Engine, OutputStream};

use crate::BlockConfig;

/// The float formats a session renders in: `f32` and `f64`. Sealed.
///
/// What a session hosts beyond its graph (the metronome, MIDI, plugins,
/// modulation, the sampler and the pool) is built of nodes that are `f32`
/// by their formats or their FFI, so an `f32` session hosts them and an `f64`
/// one is the graph, its transport and its taps. The trait is the seam: each
/// method is the part of a step that only the `f32` session has, and the `f64`
/// one has none.
pub trait SessionFloat: Float + crate::format::Sealed {}

impl SessionFloat for f32 {}
impl SessionFloat for f64 {}

pub(crate) mod sealed {
    use std::time::Instant;

    use tutti_engine::OutputStream;

    use crate::SessionEvent;

    pub trait Sealed {}

    /// What a clock's session holds to run its blocks, read by the session.
    pub trait Running: std::fmt::Debug {
        /// The stream that renders the blocks, if one does.
        fn stream(&self) -> Option<&OutputStream> {
            None
        }

        /// The clock's own share of `Phase::Poll`, after the stream's faults
        /// were read (`lost`: the stream's device left).
        fn poll(&mut self, _now: Instant, _lost: bool, _out: &mut Vec<SessionEvent>) {}
    }

    /// A [`Running`] with a stream, which a real-time clock holds.
    pub trait Streaming: Running {
        fn output_stream(&self) -> &OutputStream;
    }
}

/// What runs a session's blocks: [`Offline`], `DeviceClock` (feature
/// `device`) or [`SystemClock`]. Sealed.
///
/// A clock is a zero-sized marker; its data is [`Config`](Self::Config) while
/// the session is being built and `Running` once it is: the engine the app
/// pulls, the device, or the timer's stream.
pub trait Clock: sealed::Sealed + Sized + 'static {
    /// What a [`SessionBuilder`](crate::SessionBuilder) on this clock holds:
    /// the block sizes for a clock the session sets, the device selector for
    /// the sound card, which sets them itself.
    type Config: std::fmt::Debug;
    /// What dates the session's blocks, held by the builder: a
    /// [`BlockDates`](tutti_engine::BlockDates) where the host pulls the
    /// blocks ([`Offline`]), `()` where the OS clock times them.
    type Dates: std::fmt::Debug;
    /// What the built session holds to run its blocks.
    #[doc(hidden)]
    type Running: sealed::Running;
    /// The float format the session's audio is rendered in: the graph's
    /// [`Node<Self::Float>`](tutti_graph::Node)s and the blocks of
    /// [`Session::render`](crate::Session::render). `f32` for every clock
    /// that drives a stream; [`Offline`] and [`Detached`] carry their own.
    type Float: SessionFloat;
}

/// A clock that runs blocks on its own: `DeviceClock` and [`SystemClock`].
/// Sealed.
///
/// What both share is on `Session<K, C: RealTime>`: the stream's
/// [`faults`](crate::Session::faults), which `Session::update` reports as
/// [`SessionEvent::Fault`](crate::SessionEvent::Fault), and the listing of
/// the host's output devices.
pub trait RealTime: Clock<Float = f32, Running: sealed::Streaming> {}

/// The clock of a session the app steps: the host pulls each block with
/// [`Session::render`](crate::Session::render), from a game's or a plugin's own
/// real-time callback or from a loop faster than real time. `F` is the float
/// format of the session's audio: `f32` unless the builder says otherwise
/// (`SessionBuilder::<Offline<f64>>::default()`).
pub struct Offline<F: SessionFloat = f32>(PhantomData<fn() -> F>);

impl<F: SessionFloat> Clone for Offline<F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F: SessionFloat> Copy for Offline<F> {}

impl<F: SessionFloat> std::fmt::Debug for Offline<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Offline")
    }
}

impl<F: SessionFloat> Default for Offline<F> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

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

impl<F: SessionFloat> sealed::Sealed for Offline<F> {}
impl sealed::Sealed for SystemClock {}
#[cfg(feature = "testing")]
impl<F: SessionFloat> sealed::Sealed for Detached<F> {}
#[cfg(feature = "device")]
impl sealed::Sealed for DeviceClock {}

/// What an offline session holds: the engine its host pulls blocks from.
#[derive(Debug)]
pub struct OfflineEngine<F: SessionFloat = f32> {
    pub(crate) engine: Engine<F>,
}

/// The clock of a detached session (`SessionBuilder::build_detached`): no
/// engine, so nothing renders blocks, and the caller renders the graph by
/// hand (`Session::render_frame`) or takes its audio side. A test's seam.
#[cfg(feature = "testing")]
pub struct Detached<F: SessionFloat = f32>(PhantomData<fn() -> F>);

#[cfg(feature = "testing")]
impl<F: SessionFloat> Clone for Detached<F> {
    fn clone(&self) -> Self {
        *self
    }
}

#[cfg(feature = "testing")]
impl<F: SessionFloat> Copy for Detached<F> {}

#[cfg(feature = "testing")]
impl<F: SessionFloat> std::fmt::Debug for Detached<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Detached")
    }
}

#[cfg(feature = "testing")]
impl<F: SessionFloat> Default for Detached<F> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// A session on the detached clock: a test's, whose graph the caller renders.
#[cfg(feature = "testing")]
pub type DetachedSession<K = &'static str, F = f32> = crate::Session<K, Detached<F>>;

/// What a detached session holds: nothing, its audio side stays with the
/// graph.
#[cfg(feature = "testing")]
#[derive(Debug)]
pub struct DetachedGraph;

/// What a session on the system clock holds: the stream the timer thread runs
/// the callback on. Dropping the session stops the thread.
#[derive(Debug)]
pub struct SystemEngine {
    /// The stream's host. Out of its slot only while a clock switch runs, which
    /// puts it back before the switch returns.
    pub(crate) host: Option<tutti_engine::StreamHost>,
    /// The platform host a switch to the sound card opens the device on.
    #[cfg(feature = "device")]
    pub(crate) audio_host: tutti_cpal::AudioHost,
}

impl SystemEngine {
    pub(crate) fn host(&self) -> &tutti_engine::StreamHost {
        self.host
            .as_ref()
            .expect("the stream host is in its slot except while a clock switch runs")
    }
}

impl<F: SessionFloat> Clock for Offline<F> {
    type Config = BlockConfig;
    type Dates = tutti_engine::BlockDates;
    type Running = OfflineEngine<F>;
    type Float = F;
}

impl<F: SessionFloat> sealed::Running for OfflineEngine<F> {}

impl Clock for SystemClock {
    type Config = BlockConfig;
    type Dates = ();
    type Running = SystemEngine;
    type Float = f32;
}

impl RealTime for SystemClock {}

impl sealed::Running for SystemEngine {
    fn stream(&self) -> Option<&OutputStream> {
        Some(self.host().stream())
    }
}

impl sealed::Streaming for SystemEngine {
    fn output_stream(&self) -> &OutputStream {
        self.host().stream()
    }
}

#[cfg(feature = "device")]
impl Clock for DeviceClock {
    type Config = tutti_cpal::DeviceSelector;
    type Dates = ();
    type Running = crate::device::DeviceHandle;
    type Float = f32;
}

#[cfg(feature = "device")]
impl RealTime for DeviceClock {}

#[cfg(feature = "device")]
impl sealed::Running for crate::device::DeviceHandle {
    fn stream(&self) -> Option<&OutputStream> {
        Some(self.device().output_stream())
    }

    fn poll(&mut self, now: std::time::Instant, lost: bool, out: &mut Vec<crate::SessionEvent>) {
        self.poll_device(now, lost, out);
    }
}

#[cfg(feature = "device")]
impl sealed::Streaming for crate::device::DeviceHandle {
    fn output_stream(&self) -> &OutputStream {
        self.device().output_stream()
    }
}

#[cfg(feature = "testing")]
impl<F: SessionFloat> Clock for Detached<F> {
    type Config = ();
    type Dates = ();
    type Running = DetachedGraph;
    type Float = F;
}

#[cfg(feature = "testing")]
impl sealed::Running for DetachedGraph {}
