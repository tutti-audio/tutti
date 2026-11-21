//! How an [`OutputBlock`] is *run*, and the stream that holds the running
//! driver.
//!
//! `tutti-session`'s recorder keeps one pass apart from where it runs the same
//! way: its `PumpLoop` is one pass with no thread and no clock, which a test
//! steps by hand. Here [`OutputBlock`] is the
//! pass and [`StreamDriver`] decides where it runs: a sound card's callback
//! (`tutti-cpal`'s `CpalDriver`), or a caller's own hands
//! ([`ManualStreamDriver`](crate::ManualStreamDriver)). [`RunningStream`] is the
//! handle a started driver gives back, and [`OutputStream`] owns it together
//! with the stream's spec, fault record and latency cell.

use std::sync::Arc;

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_buffers::{Float, SampleFormat};
use tutti_units::SampleRate;

use crate::callback::{AudioCallbackState, OutputBlock};
use crate::faults::StreamFaults;
use crate::latency::StreamLatency;

/// The configuration an output stream is opened with: rate, width, sample
/// format and callback size.
///
/// A driver reads it when it opens; build one with [`new`](Self::new) for a
/// device-free stream.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OutputSpec {
    /// Frames per second the stream runs at.
    pub sample_rate: SampleRate,
    /// The device's channel count; the graph root is folded to this width.
    pub channels: ChannelLayout,
    /// The device's native sample type, which only the driver converts to: the
    /// engine renders `f32`. A backend's offset-binary unsigned type reports its
    /// signed twin of the same width.
    pub sample_format: SampleFormat,
    /// The frames every callback carries, when the stream is opened with a
    /// fixed buffer size (`None`: the backend's default, whose size is not
    /// known until it calls back). A graph is prepared with it
    /// (`tutti_graph::Prepare::with_quantum`), so a node that keeps work in
    /// step with the device (a hosted out-of-process plugin, which ships one
    /// callback's worth to its server per callback) can.
    pub quantum: Option<MaxBlock>,
}

impl OutputSpec {
    /// Creates a spec with no fixed callback size (the backend's default).
    #[must_use]
    pub fn new(
        sample_rate: SampleRate,
        channels: ChannelLayout,
        sample_format: SampleFormat,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            sample_format,
            quantum: None,
        }
    }

    /// Returns this spec with a fixed `quantum`-frame callback buffer.
    ///
    /// Keep `quantum` at or below [`MAX_FRAMES`](crate::MAX_FRAMES); a larger
    /// callback is clamped and its tail silenced. A quantum is a [`MaxBlock`],
    /// so a zero one cannot be written.
    #[must_use]
    pub fn with_quantum(mut self, quantum: MaxBlock) -> Self {
        self.quantum = Some(quantum);
        self
    }
}

/// Decides where an output stream's callback runs.
///
/// [`OutputStream`] owns the spec, the fault record and the stop, and hands a
/// driver only the [`OutputBlock`] to run.
pub trait StreamDriver<F: Float = f32> {
    /// The handle of the started stream; dropping it stops the stream.
    type Running: RunningStream + 'static;

    /// What the driver cannot open.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Opens and starts a stream at `spec`, running `block` once per callback
    /// and recording backend errors into `faults`.
    ///
    /// # Errors
    /// Whatever the driver cannot open.
    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<Self::Running, Self::Error>;
}

/// The handle a [`StreamDriver`] hands back. Dropping it stops the stream.
// `Box<Self>` rather than `self`: `OutputStream` stores this as a
// `dyn RunningStream`, and a by-value `self` cannot be called on a trait
// object.
pub trait RunningStream: Send {
    /// Stops the stream. Dropping the handle does the same; this makes the
    /// stop explicit at the call site.
    fn stop(self: Box<Self>);
}

/// Why a stream did not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartError<E> {
    /// A stream is running. It is left running and the state handed in is not
    /// adopted; stop the stream first to replace it.
    #[error("the audio stream is already running; stop it before starting another")]
    AlreadyRunning,
    /// The driver could not open the stream.
    #[error(transparent)]
    Driver(E),
}

/// One output stream: its spec, the fault record and latency cell that outlive
/// each run, and the driver's handle while it runs.
///
/// Every method runs on the control thread. The stream runs until
/// [`stop`](Self::stop) or until the stream is dropped; backend errors go to
/// [`faults`](Self::faults).
// The running stream is a `Box<dyn RunningStream>` rather than a type
// parameter so the driver choice does not leak into every type that stores an
// `OutputStream`.
pub struct OutputStream {
    spec: OutputSpec,
    faults: Arc<StreamFaults>,
    /// The output callback's reported delay, shared with each run's block.
    latency: Arc<StreamLatency>,
    running: Option<Box<dyn RunningStream>>,
}

// Hand-rolled: the running stream is not `Debug`. Reports the configuration a
// host would want in a log line; whether a stream exists is `is_running`.
impl std::fmt::Debug for OutputStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputStream")
            .field("spec", &self.spec)
            .field("is_running", &self.is_running())
            .field("faults", &self.faults.count())
            .finish_non_exhaustive()
    }
}

impl OutputStream {
    /// Creates a stream at `spec` with nothing running.
    #[must_use]
    pub fn new(spec: OutputSpec) -> Self {
        Self {
            spec,
            faults: Arc::new(StreamFaults::new()),
            latency: Arc::new(StreamLatency::new()),
            running: None,
        }
    }

    /// Returns the configuration of the stream that is playing, or that the
    /// next start would use.
    pub fn spec(&self) -> &OutputSpec {
        &self.spec
    }

    /// Replaces the spec the next start opens at.
    pub(crate) fn set_spec(&mut self, spec: OutputSpec) {
        self.spec = spec;
    }

    /// Returns the backend fault record, which survives stop and restart.
    ///
    /// Take it once at startup and poll it; a backend's error callback returns
    /// nothing, so a fault such as an unplugged device is reported only here.
    pub fn faults(&self) -> Arc<StreamFaults> {
        Arc::clone(&self.faults)
    }

    /// The delay the output callback last reported between rendering a block
    /// and its playing ([`StreamLatency`]): `None` until a callback of the
    /// running stream reports one, and again after a restart.
    pub fn latency(&self) -> Arc<StreamLatency> {
        Arc::clone(&self.latency)
    }

    /// Starts a stream at the current [`spec`](Self::spec) through `driver`.
    /// Clears [`faults`](Self::faults), so a restart after a disconnect reads
    /// healthy again, and the latency reading: the new stream has reported no
    /// delay yet.
    ///
    /// # Errors
    /// [`StartError::AlreadyRunning`] if a stream is running (it is left
    /// running and the driver is not opened); otherwise whatever the driver's
    /// [`open`](StreamDriver::open) reports.
    pub fn start_with<F: Float, D: StreamDriver<F>>(
        &mut self,
        state: Arc<AudioCallbackState<F>>,
        driver: D,
    ) -> Result<(), StartError<D::Error>> {
        if self.is_running() {
            return Err(StartError::AlreadyRunning);
        }
        self.faults.clear();
        self.latency.reset();
        let block = OutputBlock::new(state, self.spec.channels)
            .with_latency(Arc::clone(&self.latency))
            .with_faults(Arc::clone(&self.faults));
        let running = driver
            .open(&self.spec, block, Arc::clone(&self.faults))
            .map_err(StartError::Driver)?;
        self.running = Some(Box::new(running));
        Ok(())
    }

    /// [`start_with`](Self::start_with) at `spec`, which replaces the current
    /// one first. A running stream is refused before the spec is touched.
    ///
    /// # Errors
    /// As [`start_with`](Self::start_with).
    pub fn start_at<F: Float, D: StreamDriver<F>>(
        &mut self,
        state: Arc<AudioCallbackState<F>>,
        spec: OutputSpec,
        driver: D,
    ) -> Result<(), StartError<D::Error>> {
        if self.is_running() {
            return Err(StartError::AlreadyRunning);
        }
        self.spec = spec;
        self.start_with(state, driver)
    }

    /// Stops and drops the running stream, if any. Idempotent.
    ///
    /// When this returns, the callback no longer runs.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop();
        }
    }

    /// Returns the sample rate of the running stream, or of the config the
    /// next start would use.
    pub fn sample_rate(&self) -> SampleRate {
        self.spec.sample_rate
    }

    /// Returns the channel layout of the running stream, or of the config the
    /// next start would use. This is the width [`process_audio`](crate::process_audio)
    /// is handed.
    pub fn channels(&self) -> ChannelLayout {
        self.spec.channels
    }

    /// Returns whether a stream is open **and** the backend has not reported
    /// the device gone.
    ///
    /// A stream whose device was unplugged reads `false` here even though it
    /// has not been stopped; see [`StreamFaults::is_disconnected`].
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.is_some() && !self.faults.is_disconnected()
    }
}
