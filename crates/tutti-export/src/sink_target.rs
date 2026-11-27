//! A caller's [`Sink`] as a render output.

use std::error::Error;

use tutti_buffers::{ChannelLayout, Float, Interleaved, Sink, SinkFailed};

/// What closing a caller's sink failed with, its type erased.
pub(crate) type Closed = Box<dyn Error + Send + Sync>;

/// A [`Sink`] behind a trait object, finished through its box.
pub(crate) trait DynSink<F: Float>: Send {
    fn layout(&self) -> ChannelLayout;
    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed>;
    fn finish(self: Box<Self>) -> Result<(), Closed>;
}

impl<F: Float, S> DynSink<F> for S
where
    S: Sink<Sample = F> + Send,
    S::Error: Send + Sync + 'static,
{
    fn layout(&self) -> ChannelLayout {
        Sink::layout(self)
    }

    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed> {
        Sink::write(self, frames)
    }

    fn finish(self: Box<Self>) -> Result<(), Closed> {
        Sink::finalize(*self).map_err(|e| Box::new(e) as Closed)
    }
}

/// A caller's [`Sink`] one output is streamed into, as the blocks are rendered.
///
/// The sink receives the output's frames at the output's own width and the render's
/// rate and float: latency trimmed, capped at the output's length, with no fold, no
/// resample and no dither (a sink that quantizes dithers itself). A sink whose
/// [`layout`](Sink::layout) is not the width of the audio the output reads is refused
/// before anything renders ([`TargetError::SinkWidth`](crate::TargetError::SinkWidth)).
///
/// The sink is finished once, after the last block and before any file is put in place,
/// so a sink that fails keeps every file from landing. A finished sink cannot be rolled
/// back: when a later stage of the render fails, what it received stays with it. A
/// cancelled render never finishes the sink; it is dropped. A sink output cannot be
/// normalized, because a normalize needs the output kept until the whole render is
/// measured:
///
/// ```compile_fail
/// # use tutti_export::{Gain, SinkTarget};
/// # fn f(target: SinkTarget<f32>, gain: Gain) {
/// let _ = target.normalize(gain);
/// # }
/// ```
pub struct SinkTarget<F: Float = f32> {
    sink: Box<dyn DynSink<F>>,
}

impl<F: Float> SinkTarget<F> {
    /// Streams the output into `sink`.
    #[must_use]
    pub fn new<S>(sink: S) -> Self
    where
        S: Sink<Sample = F> + Send + 'static,
        S::Error: Send + Sync + 'static,
    {
        Self {
            sink: Box::new(sink),
        }
    }

    /// The sink's width.
    #[must_use]
    pub(crate) fn layout(&self) -> ChannelLayout {
        self.sink.layout()
    }

    pub(crate) fn into_sink(self) -> Box<dyn DynSink<F>> {
        self.sink
    }
}

impl<F: Float> std::fmt::Debug for SinkTarget<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkTarget")
            .field("layout", &self.layout())
            .finish_non_exhaustive()
    }
}
