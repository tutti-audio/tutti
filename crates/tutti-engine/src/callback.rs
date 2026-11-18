//! One callback's worth of output work, with no device and no thread.
//!
//! [`process_audio`] is the *inner* seam: the engine render, callable without a
//! driver. Everything around it in an output callback lives here: the
//! [`MAX_FRAMES`] clamp, the zero-fill and the write to the driver's buffer.
//! Keeping it outside the closure a backend runs is what lets a test run it
//! without a sound card.
//!
//! [`OutputBlock`] is the *outer* seam, the output side's `PumpLoop`
//! (`tutti-session`'s recorder keeps one pass's work apart from the thread that
//! runs it the same way): buffers, layout and stride, with
//! [`StreamDriver`](crate::StreamDriver) as who calls it and on what thread, and
//! [`RunningStream`](crate::RunningStream) the handle whose drop is the stop.
//!
//! [`OutputBlock::render_with`] is **not a second implementation of the
//! callback**: a device driver's closure body is one call to it, and a manual
//! driver calls the identical method.
//!
//! The `debug_assert!` on the callback size deliberately does **not** live
//! here: an over-sized buffer is a claim about a *backend's* contract, not about
//! this block, so it belongs at the driver boundary. Keeping it out is also
//! what lets a debug-build test observe the clamp instead of panicking before
//! it.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;
use tutti_buffers::InterleavedMut;
use tutti_rt::with_no_denormals;
use tutti_types::{ChannelLayout, Samples};

use crate::Engine;
use crate::faults::StreamFaults;
use crate::latency::StreamLatency;

/// Maximum frames per driver callback buffer.
///
/// The stream's mix buffer is sized to this once and never resized, so it is
/// also the largest block [`process_audio`] can be handed: a longer callback is
/// clamped and its tail silenced rather than reallocating on the audio thread.
pub const MAX_FRAMES: usize = 8192;

/// Everything the output callback reads: the [`Engine`].
///
/// Built once on the control thread and shared as an `Arc` between the host
/// that owns the stream ([`StreamHost`](crate::StreamHost), or an
/// [`OutputStream`](crate::OutputStream) on its own) and the stream's callback,
/// which passes it to [`process_audio`].
#[derive(Debug)]
pub struct AudioCallbackState {
    pub(crate) engine: Engine,
}

impl AudioCallbackState {
    /// Creates the state a stream's callback reads, on the control thread.
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }
}

/// Renders one block of the graph into `output`, an interleaved device buffer
/// that carries its own width.
///
/// The graph's root outputs are folded to that width (see [`Engine::process`]),
/// and denormals are flushed for the duration of the call. This is the render
/// the output stream runs each callback; call it directly only on a state no
/// stream renders.
///
/// # Real-time
///
/// **Runs on the audio thread. Must not allocate, lock, or block.** Every
/// buffer it touches is sized once at stream build to [`MAX_FRAMES`]; a longer
/// callback is clamped and its tail silenced rather than reallocating here.
/// `tests/no_alloc/block_no_alloc.rs` gates this against a disabled allocator.
///
/// # Panics
///
/// If a node, the executor or the engine panics, the panic propagates from
/// here: this function is not a guard. A stream's callback runs it through
/// [`OutputBlock::render_with`], which catches the panic, writes silence and
/// stops rendering (see there); a caller on a thread of its own that a C frame
/// called must do the same.
#[inline]
pub fn process_audio(state: &AudioCallbackState, output: &mut InterleavedMut<'_>) {
    with_no_denormals(|| state.engine.process(output));
}

/// [`process_audio`] for a block dated by the caller: the block clock records
/// `os_ns` as the OS time the block began ([`Engine::process_at`]).
#[inline]
fn process_audio_at(state: &AudioCallbackState, output: &mut InterleavedMut<'_>, os_ns: i64) {
    with_no_denormals(|| state.engine.process_at(output, os_ns));
}

/// The working set one output stream's callback reads, sized once.
///
/// Every buffer here is allocated in [`OutputBlock::new`], on the control
/// thread, and never resized: a callback larger than [`MAX_FRAMES`] is
/// clamped in [`render_with`](OutputBlock::render_with) and its tail silenced
/// rather than reallocating on the audio thread.
pub struct OutputBlock {
    state: Arc<AudioCallbackState>,
    /// The device's true channel layout. The engine folds the graph root to
    /// this width (declared with [`Engine::support_output`](crate::Engine::support_output)
    /// when the block is built). A device wider than
    /// [`MAX_CHANNELS`](tutti_types::MAX_CHANNELS) is refused when its config is
    /// read, never truncated.
    layout: ChannelLayout,
    /// The interleave stride for both the mix buffer and the device buffer,
    /// derived ONCE here — never inside the per-frame loops.
    channels: usize,
    buffer: Vec<f32>,
    last_rendered_frames: usize,
    /// Where each callback's reported delay goes ([`render_delayed_with`](Self::render_delayed_with)).
    latency: Arc<StreamLatency>,
    /// Where a panic of the render is recorded.
    faults: Arc<StreamFaults>,
    /// Set by a panic in the render and never cleared: the engine's state may
    /// be torn mid-block, so this block writes silence from then on.
    failed: bool,
}

impl std::fmt::Debug for OutputBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputBlock")
            .field("state", &self.state)
            .field("layout", &self.layout)
            .field("channels", &self.channels)
            .field("last_rendered_frames", &self.last_rendered_frames)
            .finish_non_exhaustive()
    }
}

impl OutputBlock {
    /// Allocate the callback's working set for a device of `layout` width.
    ///
    /// Control-thread only. This is the only allocation on the output path,
    /// and the one place the device's width reaches the engine: it builds the
    /// engine's fold from the graph root to the device
    /// ([`Engine::support_output`](crate::Engine::support_output)), so the
    /// callback never does.
    ///
    /// # Panics
    ///
    /// If `layout` has no channels: a device has at least one.
    pub fn new(state: Arc<AudioCallbackState>, layout: ChannelLayout) -> Self {
        state
            .engine
            .support_output(layout)
            .expect("a device has at least one channel");
        let channels = layout.count() as usize;
        Self {
            state,
            layout,
            channels,
            buffer: vec![0.0f32; layout.interleaved_len(Samples(MAX_FRAMES))],
            last_rendered_frames: 0,
            latency: Arc::new(StreamLatency::new()),
            faults: Arc::new(StreamFaults::new()),
            failed: false,
        }
    }

    /// This block recording a panic of its render into `faults`, the stream's
    /// record, instead of one of its own.
    #[must_use]
    pub(crate) fn with_faults(mut self, faults: Arc<StreamFaults>) -> Self {
        self.faults = faults;
        self
    }

    /// This block reporting each callback's delay into `latency` instead of a
    /// cell of its own: the engine's, which outlives the block.
    #[must_use]
    pub(crate) fn with_latency(mut self, latency: Arc<StreamLatency>) -> Self {
        self.latency = latency;
        self
    }

    /// The device width this block was sized for.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The interleave stride — `layout().count()`, hoisted.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames the last render actually produced, after the [`MAX_FRAMES`]
    /// clamp.
    ///
    /// Exists so a test can assert the clamp happened rather than infer it
    /// from a silent tail.
    pub fn last_rendered_frames(&self) -> usize {
        self.last_rendered_frames
    }

    /// [`render_with`](Self::render_with), first recording the delay the
    /// callback's timestamp reports between now and when this block plays
    /// (`None`: the backend gave none, which clears the reading). A stream
    /// driver calls this with what its backend stamped; wait-free like
    /// `render_with`.
    #[inline]
    pub fn render_delayed_with<T: Copy>(
        &mut self,
        data: &mut [T],
        delay: Option<Duration>,
        convert: impl Fn(f32) -> T,
    ) {
        self.latency.record(delay);
        self.render_with(data, convert);
    }

    /// [`render_with`](Self::render_with) into an `f32` device buffer.
    #[inline]
    pub fn render(&mut self, data: &mut [f32]) {
        self.render_with(data, |x| x);
    }

    /// Render one block and write it to `data`, each sample made from the
    /// rendered `f32` by `convert` (the driver's sample format).
    ///
    /// This is the entire output callback. A device driver calls it with
    /// whatever buffer the backend handed over; a manual driver calls it with a
    /// buffer of the caller's choosing. `convert` is the driver's one
    /// conversion from the engine's nominal `f32` to its device's type; it runs
    /// on the audio thread, so it does not allocate, lock or wait, and it maps
    /// `0.0` to the format's silence.
    ///
    /// # Panics in the render
    ///
    /// None escapes: this runs in a device callback that a C frame called, and
    /// unwinding through one aborts or is undefined behaviour. A panic in a
    /// node, the executor or the engine is caught here, `data` is filled with
    /// silence, and the block is latched: every later call writes silence and
    /// never calls the engine again, because the panic may have left its state
    /// half-written. The stream's [`StreamFaults`] records it as
    /// [`StreamFaultKind::CallbackPanicked`](crate::StreamFaultKind::CallbackPanicked)
    /// and reads as no longer running; a new stream (a restart) builds a new
    /// block, which renders again. The panic hook still runs first and may
    /// allocate; nothing the guard itself does does.
    ///
    /// # Real-time
    ///
    /// Runs on the audio thread. Must not allocate, lock, or block, which
    /// `tests/no_alloc/block_no_alloc.rs` gates.
    #[inline]
    pub fn render_with<T: Copy>(&mut self, data: &mut [T], convert: impl Fn(f32) -> T) {
        self.render_stamped(data, None, convert);
    }

    /// [`render_delayed_with`](Self::render_delayed_with) into an `f32` buffer
    /// for a block due at `os_ns`, a [`monotonic_ns`](tutti_rt::monotonic_ns)
    /// reading: the block clock dates the block by it, not by the time the
    /// render began ([`Engine::process_at`]). A driver that renders to a
    /// schedule calls this with the block's deadline. Wait-free like
    /// `render_with`, with the same panic latch.
    #[inline]
    pub(crate) fn render_at(&mut self, data: &mut [f32], os_ns: i64, delay: Option<Duration>) {
        self.latency.record(delay);
        self.render_stamped(data, Some(os_ns), |x| x);
    }

    #[inline]
    fn render_stamped<T: Copy>(
        &mut self,
        data: &mut [T],
        stamp: Option<i64>,
        convert: impl Fn(f32) -> T,
    ) {
        let silence = convert(0.0);
        if self.failed {
            data.fill(silence);
            return;
        }
        let rendered = catch_unwind(AssertUnwindSafe(|| {
            self.render_unguarded(data, stamp, &convert, silence)
        }));
        if let Err(payload) = rendered {
            self.failed = true;
            data.fill(silence);
            self.faults.record_panic("the output callback panicked");
            // Dropping the payload would free on the audio thread, and a
            // payload's own drop may panic: leak it, once per stream.
            std::mem::forget(payload);
        }
    }

    fn render_unguarded<T: Copy>(
        &mut self,
        data: &mut [T],
        stamp: Option<i64>,
        convert: &impl Fn(f32) -> T,
        silence: T,
    ) {
        let channels = self.channels;
        // Clamp so we never allocate. If a backend hands us a larger buffer we
        // process the head and write silence to the tail. The assertion that
        // this should not happen lives at the driver boundary, not here.
        let frames = self
            .layout
            .from_interleaved_len(data.len())
            .min(Samples(MAX_FRAMES));
        self.last_rendered_frames = frames.get();

        let mix = &mut self.buffer[..self.layout.interleaved_len(frames)];
        // Zero before rendering: the previous callback's contents are not
        // meaningful input for the graph.
        mix.fill(0.0);
        // `frames` frames of the device's layout: whole frames
        // of a width the device opened with, so the view is always built.
        let Ok(mut mix) = InterleavedMut::new(mix, self.layout) else {
            return;
        };
        match stamp {
            Some(os_ns) => process_audio_at(&self.state, &mut mix, os_ns),
            None => process_audio(&self.state, &mut mix),
        }
        // Back to a flat slice for the device write.
        let mix = mix.as_interleaved().samples();

        write_output(data, channels, mix, frames.get(), convert, silence);
    }
}

/// Convert the rendered f32 mix into the driver's sample type.
///
/// `data` stays a bare `&mut [T]` and `channels` a bare `usize`: `T` is
/// whatever the backend's buffer holds, including offset-binary unsigned
/// formats that no `tutti_buffers::Sample` is, so `InterleavedMut` cannot
/// describe every destination. The only arithmetic in this function is
/// `i / channels`, and the width it needs is the *source's*, which the caller
/// already reads off the `InterleavedMut` it built. The vocabulary stops at the
/// backend's buffer, as it does at the C ABI and wire boundaries.
///
/// Private on purpose: it is reached through [`OutputBlock::render_with`], which
/// is how a driver reaches it. A parallel public conversion helper would be a
/// second implementation of the matrix under test.
#[inline]
fn write_output<T: Copy>(
    data: &mut [T],
    channels: usize,
    output: &[f32],
    rendered_frames: usize,
    convert: &impl Fn(f32) -> T,
    silence: T,
) {
    // `output` is already `channels`-wide interleaved (the engine folded the
    // graph root to the device width). Copy every channel through; frames past
    // what we rendered (an over-sized callback) get silence.
    for (i, sample) in data.iter_mut().enumerate() {
        let frame = i / channels;
        *sample = if frame < rendered_frames {
            convert(output[i])
        } else {
            silence
        };
    }
}
