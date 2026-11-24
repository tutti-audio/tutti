//! A stream driver with no thread and no device: the caller runs the callbacks.
//!
//! [`ManualStreamDriver`] / [`ManualStream`] let a test (or a tool) call the
//! [`OutputBlock`] by hand through the same [`StreamDriver`] seam a device
//! driver runs it through, so a manual stream runs exactly the callback a device
//! runs.

use std::convert::Infallible;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_buffers::{Float, Sample};
use tutti_rt::Mutex;
use tutti_units::Samples;

use crate::callback::OutputBlock;
use crate::faults::{BackendErrorKind, StreamFaults};
use crate::stream::{OpenedOutput, OutputSpec, RunningStream, StreamDriver};

/// A driver that runs no callbacks of its own: the caller runs them through
/// the paired [`ManualStream`].
///
/// For device-free tests of a host's lifecycle: pass it to
/// [`OutputStream::start_with`](crate::OutputStream::start_with) or
/// [`StreamHost::start_with`](crate::StreamHost::start_with) and render blocks
/// with [`ManualStream::render_block`].
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use tutti_buffers::SampleFormat;
/// use tutti_engine::{
///     AudioCallbackState, Engine, ManualStreamDriver, OutputSpec, OutputStream,
/// };
/// use tutti_graph::{Editor, Prepare};
/// use tutti_timeline::Transport;
/// use tutti_units::{SampleRate, Samples};
/// use tutti_buffers::{ChannelLayout, MaxBlock};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let rate = SampleRate::SR_48K;
/// let transport = Transport::new(rate);
/// let (mut editor, executor) = Editor::new(Prepare::new(rate, MaxBlock::of(Samples::new(512))));
/// let engine = Engine::new(&transport, &mut editor, executor)?;
/// let state = Arc::new(AudioCallbackState::new(engine));
///
/// let spec = OutputSpec::new(rate, ChannelLayout::STEREO, SampleFormat::F32);
/// let mut output = OutputStream::new(spec);
/// let (driver, stream) = ManualStreamDriver::new();
/// output.start_with(state, driver)?;
///
/// let block = stream.render_block(Samples::new(256)).expect("the stream is open");
/// assert_eq!(block.len(), 256 * 2);
///
/// output.stop();
/// assert!(!stream.is_open());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ManualStreamDriver<F: Float = f32> {
    slot: Arc<Mutex<Option<Open<F>>>>,
}

/// An open stream: the callback to run and the record its faults go to.
#[derive(Debug)]
struct Open<F: Float> {
    block: OutputBlock<F>,
    faults: Arc<StreamFaults>,
}

/// The caller's half of a [`ManualStreamDriver`], used to run callbacks by
/// hand.
///
/// Once the stream is stopped every method returns `None` (or `false`), which
/// is how a test observes the stop. Each call takes a mutex around the block;
/// this type is for tests and tools, not the audio thread.
#[derive(Debug)]
pub struct ManualStream<F: Float = f32> {
    slot: Arc<Mutex<Option<Open<F>>>>,
}

impl<F: Float> ManualStreamDriver<F> {
    /// Creates a driver and the handle that runs its callbacks.
    #[allow(clippy::new_without_default, reason = "returns a pair, not Self")]
    #[must_use]
    pub fn new() -> (Self, ManualStream<F>) {
        let slot = Arc::new(Mutex::new(None));
        (
            Self {
                slot: Arc::clone(&slot),
            },
            ManualStream { slot },
        )
    }
}

impl<F: Float> ManualStream<F> {
    fn with_block<R>(&self, f: impl FnOnce(&mut OutputBlock<F>) -> R) -> Option<R> {
        let mut guard = self.slot.lock();
        guard.as_mut().map(|open| f(&mut open.block))
    }

    /// Runs one callback, exactly as a device driver would, into a buffer of
    /// any [`Sample`] format ([`OutputBlock::render`]). Returns `None` if no
    /// stream is open.
    pub fn run_callback<S: Sample>(&self, data: &mut [S]) -> Option<()> {
        self.with_block(|b| b.render(data))
    }

    /// Renders `frames` frames at the stream's width and returns the
    /// interleaved buffer in the graph's format, or `None` if no stream is
    /// open. Allocates the returned buffer.
    pub fn render_block(&self, frames: Samples) -> Option<Vec<F>> {
        self.render_block_inner(frames, None, None)
    }

    /// [`render_block`](Self::render_block) as a callback whose timestamp
    /// reports `delay` between the callback and the block's playing, as a
    /// backend's callback info does: what
    /// [`OutputStream::latency`](crate::OutputStream::latency) then reads.
    pub fn render_block_delayed(&self, frames: Samples, delay: Duration) -> Option<Vec<F>> {
        self.render_block_inner(frames, None, Some(delay))
    }

    /// [`render_block_delayed`](Self::render_block_delayed) as a callback the
    /// caller dates: the block clock records `at` as the OS time the block
    /// began, where the other renders read the clock themselves, as a device
    /// callback stamped with its own timestamp, or a timer's block stamped with
    /// the instant it was due. `delay` is the delay the callback reports
    /// (`None`: none, which clears the reading). Returns `None` if no stream is
    /// open, or if `at` is beyond what the block clock's nanoseconds hold
    /// (nothing is rendered).
    pub fn render_block_at(
        &self,
        frames: Samples,
        at: Instant,
        delay: Option<Duration>,
    ) -> Option<Vec<F>> {
        let os_ns = tutti_platform::monotonic_ns_of(at)?;
        self.render_block_inner(frames, Some(os_ns), delay)
    }

    fn render_block_inner(
        &self,
        frames: Samples,
        os_ns: Option<i64>,
        delay: Option<Duration>,
    ) -> Option<Vec<F>> {
        let layout = self.with_block(|b| b.layout())?;
        let mut out = vec![F::ZERO; layout.interleaved_len(frames)];
        self.with_block(|b| match os_ns {
            Some(os_ns) => b.render_at(&mut out, os_ns, delay),
            None => b.render_delayed_with(&mut out, delay, |x| x),
        })?;
        if let Some(open) = self.slot.lock().as_ref() {
            open.faults.delivered();
        }
        Some(out)
    }

    /// Returns the frames the last callback actually rendered: its own, at most
    /// [`MAX_FRAMES`](crate::MAX_FRAMES).
    pub fn last_rendered_frames(&self) -> Option<usize> {
        self.with_block(|b| b.last_rendered_frames())
    }

    /// Returns the channel count this stream was opened at.
    pub fn channels(&self) -> Option<usize> {
        self.with_block(|b| b.channels())
    }

    /// Delivers a backend error exactly as a driver's error callback would
    /// at the instant `now` ([`StreamFaults::record`]), so a test can exercise
    /// the fault path without unplugging a device. Does nothing while no
    /// stream is open.
    pub fn fail(&self, kind: BackendErrorKind, err: &dyn Display, now: Instant) {
        if let Some(open) = self.slot.lock().as_ref() {
            open.faults.record(kind, err, now);
        }
    }

    /// Returns whether a stream is currently open on this handle.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.slot.lock().is_some()
    }
}

/// The running handle of a [`ManualStreamDriver`] stream. Dropping it stops
/// the stream: every [`ManualStream`] call then returns `None`.
#[derive(Debug)]
pub struct ManualRunning<F: Float = f32> {
    slot: Arc<Mutex<Option<Open<F>>>>,
}

impl<F: Float> RunningStream for ManualRunning<F> {
    fn stop(self: Box<Self>) {
        // Drop does it.
    }
}

impl<F: Float> Drop for ManualRunning<F> {
    fn drop(&mut self) {
        *self.slot.lock() = None;
    }
}

impl<F: Float> StreamDriver<F> for ManualStreamDriver<F> {
    type Running = ManualRunning<F>;
    type Error = Infallible;

    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<OpenedOutput<ManualRunning<F>>, Infallible> {
        // The block and its fault sink reach the handle here rather than
        // through a separate wiring call: `open` is the one place both the
        // driver and the sink are in scope, and a generic `start_with` cannot
        // know it is holding a manual driver.
        *self.slot.lock() = Some(Open { block, faults });
        Ok(OpenedOutput {
            stream: ManualRunning { slot: self.slot },
            quantum: spec.quantum,
        })
    }
}
