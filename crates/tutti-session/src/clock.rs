//! [`Clock`]: what runs a session's blocks, and so the session's type.
//!
//! Every session differs at its main output by one thing: the app steps an
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
pub type SystemSession<K = &'static str, F = f32> = crate::Session<K, SystemClock<F>>;

use std::marker::PhantomData;

use tutti_buffers::Float;
use tutti_engine::{Engine, OutputStream};

use crate::BlockConfig;

/// The float formats a session renders in: `f32` and `f64`. Sealed.
///
/// The graph, its taps, meters and inlets, the transport, the metronome, MIDI,
/// the offline, system and device clocks and the switch between them, a bounce,
/// the session's background work (a sampler stream, an analysis), modulation, the
/// capture inputs and the takes work in either, as do the instruments. Only the
/// plugins are `f32` (by their FFI) and hosted by an `f32` session alone: each is
/// an `impl` on `Session<K, C>` where `C: Clock<Float = f32>`, and the trait is
/// the seam where an `f64` session has nothing to do.
pub trait SessionFloat: Float + crate::format::Sealed {}

impl SessionFloat for f32 {}
impl SessionFloat for f64 {}

pub(crate) mod sealed {
    use tutti_platform::Instant;

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
    /// [`Session::render`](crate::Session::render): the clock's own `F`.
    type Float: SessionFloat;
}

/// A clock that runs blocks on its own: `DeviceClock` and [`SystemClock`].
/// Sealed.
///
/// What both share is on `Session<K, C: RealTime>`: the stream's
/// [`faults`](crate::Session::faults), which `Session::update` reports as
/// [`SessionEvent::Fault`](crate::SessionEvent::Fault), and the listing of
/// the host's output devices.
pub trait RealTime: Clock<Running: sealed::Streaming> {}

/// The clock of a session the app steps: the host pulls each block with
/// [`Session::render`](crate::Session::render), from a game's or a plugin's own
/// real-time callback or from a loop faster than real time. `F` is the float
/// format of the session's audio: `f32` unless the builder says otherwise
/// (`Session::builder().float::<f64>()`).
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
pub struct SystemClock<F: SessionFloat = f32>(PhantomData<fn() -> F>);

impl<F: SessionFloat> Clone for SystemClock<F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F: SessionFloat> Copy for SystemClock<F> {}

impl<F: SessionFloat> std::fmt::Debug for SystemClock<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SystemClock")
    }
}

impl<F: SessionFloat> Default for SystemClock<F> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// The clock of a session a sound card's callback steps
/// ([`SessionBuilder::device`](crate::SessionBuilder::device)): the device sets
/// the rate, the width and the callback size.
#[cfg(feature = "device")]
pub struct DeviceClock<F: SessionFloat = f32>(PhantomData<fn() -> F>);

#[cfg(feature = "device")]
impl<F: SessionFloat> Clone for DeviceClock<F> {
    fn clone(&self) -> Self {
        *self
    }
}

#[cfg(feature = "device")]
impl<F: SessionFloat> Copy for DeviceClock<F> {}

#[cfg(feature = "device")]
impl<F: SessionFloat> std::fmt::Debug for DeviceClock<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceClock")
    }
}

#[cfg(feature = "device")]
impl<F: SessionFloat> Default for DeviceClock<F> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<F: SessionFloat> sealed::Sealed for Offline<F> {}
impl<F: SessionFloat> sealed::Sealed for SystemClock<F> {}
#[cfg(feature = "testing")]
impl<F: SessionFloat> sealed::Sealed for Detached<F> {}
#[cfg(feature = "device")]
impl<F: SessionFloat> sealed::Sealed for DeviceClock<F> {}

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
pub struct SystemEngine<F: SessionFloat = f32> {
    /// The stream's host. Out of its slot only while a clock switch runs, which
    /// puts it back before the switch returns.
    pub(crate) host: Option<tutti_engine::StreamHost<F>>,
    /// The platform host a switch to the sound card opens the device on.
    #[cfg(feature = "device")]
    pub(crate) audio_host: tutti_cpal::AudioHost,
}

impl<F: SessionFloat> SystemEngine<F> {
    pub(crate) fn host(&self) -> &tutti_engine::StreamHost<F> {
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

impl<F: SessionFloat> Clock for SystemClock<F> {
    type Config = BlockConfig;
    type Dates = ();
    type Running = SystemEngine<F>;
    type Float = F;
}

impl<F: SessionFloat> RealTime for SystemClock<F> {}

impl<F: SessionFloat> sealed::Running for SystemEngine<F> {
    fn stream(&self) -> Option<&OutputStream> {
        Some(self.host().stream())
    }
}

impl<F: SessionFloat> sealed::Streaming for SystemEngine<F> {
    fn output_stream(&self) -> &OutputStream {
        self.host().stream()
    }
}

#[cfg(feature = "device")]
impl<F: SessionFloat> Clock for DeviceClock<F> {
    type Config = tutti_cpal::OutputConfig;
    type Dates = ();
    type Running = crate::device::DeviceHandle<F>;
    type Float = F;
}

#[cfg(feature = "device")]
impl<F: SessionFloat> RealTime for DeviceClock<F> {}

#[cfg(feature = "device")]
impl<F: SessionFloat> sealed::Running for crate::device::DeviceHandle<F> {
    fn stream(&self) -> Option<&OutputStream> {
        Some(self.device().output_stream())
    }

    fn poll(
        &mut self,
        now: tutti_platform::Instant,
        lost: bool,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        self.poll_device(now, lost, out);
    }
}

#[cfg(feature = "device")]
impl<F: SessionFloat> sealed::Streaming for crate::device::DeviceHandle<F> {
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
