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
use tutti_units::{SampleRate, Samples};

use crate::callback::{AudioCallbackState, MAX_FRAMES, OutputBlock};
use crate::faults::StreamFaults;
use crate::latency::StreamLatency;

tutti_units::ranged! {
    /// The frames of one device callback, from one to [`MAX_FRAMES`]: the
    /// largest buffer an output stream's callback renders whole.
    OutputQuantum(usize), QuantumOutOfRange, "from one frame to MAX_FRAMES",
    |raw| raw >= 1 && raw <= MAX_FRAMES
}

impl OutputQuantum {
    /// This quantum as the longest block a graph is prepared for.
    #[must_use]
    pub const fn max_block(self) -> MaxBlock {
        MaxBlock::of(Samples::new(self.get()))
    }

    /// This quantum as a frame count.
    #[must_use]
    pub const fn samples(self) -> Samples {
        Samples::new(self.get())
    }
}

impl Eq for OutputQuantum {}

impl TryFrom<MaxBlock> for OutputQuantum {
    type Error = QuantumOutOfRange;

    /// The block as a quantum, refused when it is longer than [`MAX_FRAMES`].
    fn try_from(block: MaxBlock) -> Result<Self, QuantumOutOfRange> {
        Self::try_from(block.get())
    }
}

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
    /// The device's channel count; the graph's main output is folded to this width.
    pub channels: ChannelLayout,
    /// The device's native sample type, which only the driver converts to: the
    /// engine renders `f32`. A backend's offset-binary unsigned type reports its
    /// signed twin of the same width.
    pub sample_format: SampleFormat,
    /// The frames every callback carries (`None`: not known).
    ///
    /// Before a stream is open it is the size asked for: `Some` opens the
    /// stream with that fixed buffer, `None` with the backend's default. Once
    /// [`OutputStream::start_with`] has opened a stream it is what the driver
    /// reports the stream opened at ([`OpenedOutput::quantum`]), which can
    /// differ from the request or be `None`; a stop returns the request. A
    /// graph is prepared with it (`tutti_graph::Prepare::with_quantum`), so a
    /// node that keeps work in step with the device (a hosted out-of-process
    /// plugin, which ships one callback's worth to its server per callback)
    /// can.
    ///
    /// What a driver's report means is the driver's: a fixed buffer size is a
    /// request a backend may round or ignore, and the size a backend reports
    /// is an estimate that is not enforced per callback.
    pub quantum: Option<OutputQuantum>,
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
    /// An [`OutputQuantum`] is from one frame to [`MAX_FRAMES`],
    /// so a quantum the callback could not render whole cannot be written.
    #[must_use]
    pub fn with_quantum(mut self, quantum: OutputQuantum) -> Self {
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
    /// and recording backend errors into `faults`. Answers the stream and the
    /// callback size it runs at, which is `spec.quantum` when the driver runs
    /// at the size it was asked for.
    ///
    /// # Errors
    /// Whatever the driver cannot open.
    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<OpenedOutput<Self::Running>, Self::Error>;
}

/// What a [`StreamDriver`] answers when it opens: the running stream and the
/// callback size it runs at.
///
/// A plain record with no invariant between its fields, so both are public.
#[derive(Debug)]
pub struct OpenedOutput<R> {
    /// The handle of the started stream.
    pub stream: R,
    /// The frames the stream's callbacks carry, as the driver reports them
    /// (`None`: the backend cannot say). It becomes
    /// [`OutputSpec::quantum`] while the stream is open.
    pub quantum: Option<OutputQuantum>,
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
    running: Option<Running>,
}

/// An open stream, with the quantum its spec asked for: the spec carries the
/// opened one while the stream is held, and [`OutputStream::stop`] puts the
/// request back.
struct Running {
    stream: Box<dyn RunningStream>,
    requested: Option<OutputQuantum>,
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
    /// next start would use. Its [`quantum`](OutputSpec::quantum) is the one
    /// the open stream reports, and the request when none is open.
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
    /// [`open`](StreamDriver::open) reports. A stream whose device was lost is
    /// released before the driver opens.
    pub fn start_with<F: Float, D: StreamDriver<F>>(
        &mut self,
        state: Arc<AudioCallbackState<F>>,
        driver: D,
    ) -> Result<(), StartError<D::Error>> {
        if self.is_running() {
            return Err(StartError::AlreadyRunning);
        }
        // A stream still held here is one whose device was lost: it is released
        // before the next opens, so two are never held.
        self.stop();
        self.faults.clear();
        self.latency.reset();
        let block = OutputBlock::new(state, self.spec.channels)
            .with_latency(Arc::clone(&self.latency))
            .with_faults(Arc::clone(&self.faults));
        let opened = driver
            .open(&self.spec, block, Arc::clone(&self.faults))
            .map_err(StartError::Driver)?;
        self.running = Some(Running {
            stream: Box::new(opened.stream),
            requested: self.spec.quantum,
        });
        self.spec.quantum = opened.quantum;
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
        if let Some(Running { stream, requested }) = self.running.take() {
            stream.stop();
            self.spec.quantum = requested;
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
