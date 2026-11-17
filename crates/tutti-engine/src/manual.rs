//! A stream driver with no thread and no device: the caller runs the callbacks.
//!
//! [`ManualStreamDriver`] / [`ManualStream`] let a test (or a tool) call the
//! [`OutputBlock`] by hand through the same [`StreamDriver`] seam a device
//! driver runs it through, so a manual stream runs exactly the callback a device
//! runs.

use std::convert::Infallible;
use std::fmt::Display;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_rt::LockExt;

use crate::callback::OutputBlock;
use crate::faults::{BackendErrorKind, StreamFaults};
use crate::stream::{OutputSpec, RunningStream, StreamDriver};

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
/// use tutti_transport::Transport;
/// use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let rate = SampleRate::SR_48K;
/// let transport = Transport::new(rate);
/// let (mut editor, executor) = Editor::new(Prepare::new(rate, MaxBlock::of(Samples(512))));
/// let engine = Engine::new(&transport, &mut editor, executor, tutti_engine::BlockDates::Os)?;
/// let state = Arc::new(AudioCallbackState::new(engine));
///
/// let spec = OutputSpec::new(rate, ChannelLayout::STEREO, SampleFormat::F32);
/// let mut output = OutputStream::new(spec);
/// let (driver, stream) = ManualStreamDriver::new();
/// output.start_with(state, driver)?;
///
/// let block = stream.render_block(256).expect("the stream is open");
/// assert_eq!(block.len(), 256 * 2);
///
/// output.stop();
/// assert!(!stream.is_open());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ManualStreamDriver {
    slot: Arc<Mutex<Option<OutputBlock>>>,
    faults: Arc<Mutex<Option<Arc<StreamFaults>>>>,
}

/// The caller's half of a [`ManualStreamDriver`], used to run callbacks by
/// hand.
///
/// Once the stream is stopped every method returns `None` (or `false`), which
/// is how a test observes the stop. Each call takes a mutex around the block;
/// this type is for tests and tools, not the audio thread.
#[derive(Debug)]
pub struct ManualStream {
    slot: Arc<Mutex<Option<OutputBlock>>>,
    faults: Arc<Mutex<Option<Arc<StreamFaults>>>>,
}

impl ManualStreamDriver {
    /// Creates a driver and the handle that runs its callbacks.
    #[allow(clippy::new_without_default, reason = "returns a pair, not Self")]
    #[must_use]
    pub fn new() -> (Self, ManualStream) {
        let slot = Arc::new(Mutex::new(None));
        let faults = Arc::new(Mutex::new(None));
        (
            Self {
                slot: Arc::clone(&slot),
                faults: Arc::clone(&faults),
            },
            ManualStream { slot, faults },
        )
    }
}

impl ManualStream {
    fn with_block<R>(&self, f: impl FnOnce(&mut OutputBlock) -> R) -> Option<R> {
        let mut guard = self.slot.lock_recover();
        guard.as_mut().map(f)
    }

    /// Runs one callback, exactly as a device driver would, into a buffer of
    /// `f32`. Returns `None` if no stream is open.
    pub fn callback_once(&self, data: &mut [f32]) -> Option<()> {
        self.with_block(|b| b.render(data))
    }

    /// Renders `frames` frames at the stream's width and returns the
    /// interleaved `f32` buffer, or `None` if no stream is open. Allocates the
    /// returned buffer.
    pub fn render_block(&self, frames: usize) -> Option<Vec<f32>> {
        self.render_block_inner(frames, None)
    }

    /// [`render_block`](Self::render_block) as a callback whose timestamp
    /// reports `delay` between the callback and the block's playing, as a
    /// backend's callback info does: what
    /// [`OutputStream::latency`](crate::OutputStream::latency) then reads.
    pub fn render_block_delayed(&self, frames: usize, delay: Duration) -> Option<Vec<f32>> {
        self.render_block_inner(frames, Some(delay))
    }

    fn render_block_inner(&self, frames: usize, delay: Option<Duration>) -> Option<Vec<f32>> {
        let channels = self.with_block(|b| b.channels())?;
        let mut out = vec![0.0f32; frames * channels];
        self.with_block(|b| b.render_delayed_with(&mut out, delay, |x| x))?;
        if let Some(faults) = self.faults.lock_recover().as_ref() {
            faults.delivered();
        }
        Some(out)
    }

    /// Returns the frames the last callback actually rendered, after the
    /// [`MAX_FRAMES`](crate::MAX_FRAMES) clamp.
    pub fn last_rendered_frames(&self) -> Option<usize> {
        self.with_block(|b| b.last_rendered_frames())
    }

    /// Returns the channel count this stream was opened at.
    pub fn channels(&self) -> Option<usize> {
        self.with_block(|b| b.channels())
    }

    /// Delivers a backend error exactly as a driver's error callback would
    /// at the instant `now` ([`StreamFaults::record`]), so a test can exercise
    /// the fault path without unplugging a device. Does nothing before the
    /// stream is first opened.
    pub fn fail(&self, kind: BackendErrorKind, err: &dyn Display, now: Instant) {
        if let Some(faults) = self.faults.lock_recover().as_ref() {
            faults.record(kind, err, now);
        }
    }

    /// Returns whether a stream is currently open on this handle.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.slot.lock_recover().is_some()
    }
}

/// The running handle of a [`ManualStreamDriver`] stream. Dropping it stops
/// the stream: every [`ManualStream`] call then returns `None`.
#[derive(Debug)]
pub struct ManualRunning {
    slot: Arc<Mutex<Option<OutputBlock>>>,
}

impl RunningStream for ManualRunning {
    fn stop(self: Box<Self>) {
        // Drop does it.
    }
}

impl Drop for ManualRunning {
    fn drop(&mut self) {
        *self.slot.lock_recover() = None;
    }
}

impl StreamDriver for ManualStreamDriver {
    type Running = ManualRunning;
    type Error = Infallible;

    fn open(
        self,
        _spec: &OutputSpec,
        block: OutputBlock,
        faults: Arc<StreamFaults>,
    ) -> Result<ManualRunning, Infallible> {
        // The stream's sink reaches the handle here rather than through a
        // separate wiring call: `open` is the one place both the driver and
        // the sink are in scope, and a generic `start_with` cannot know it is
        // holding a manual driver.
        *self.faults.lock_recover() = Some(faults);
        *self.slot.lock_recover() = Some(block);
        Ok(ManualRunning { slot: self.slot })
    }
}
