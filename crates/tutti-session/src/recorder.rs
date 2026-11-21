//! Recording: a source pumped into a sink, one pass at a time.
//!
//! Recording is a source pulled into a sink: pull a block, write it.
//! [`PumpLoop`] is one pass of that with no thread and no clock: an app steps it
//! on its own loop. It joins any [`Source`] to any [`Sink`]: a
//! `tutti_codec::FileIn` to a file, a graph tap's
//! [`TapReader`](tutti_graph::TapReader) to a `tutti_codec::FileOut`, a
//! capture device to either.
//!
//! - [`PumpLoop::new`] refuses a source and sink whose channel widths disagree,
//!   before any frame moves. The same check is a `debug_assert` in the pass
//!   itself (`pump`, private: nothing else calls it).
//! - A recording does not dither unless asked. To dither a take as an export
//!   does, open the file for its integer format and wrap it in one line:
//!   `PumpLoop::new(src, DitherSink::<_, f32>::new(FileOut::<i16>::new(&spec, File::create(path)?)?, Some(shape)), capacity)`.
//!   The stage adds the noise at one LSB of `i16`, converts and counts the values it clipped.
//! - The scratch buffer is allocated once, so a pass never allocates.
//! - An empty pull means "back off", "finished" or "broken" according to the
//!   source's own [`Pull`] verdict. `Starved` sources (every microphone, every
//!   tap) never end on their own; the caller stops stepping them and
//!   [`finalize`](PumpLoop::finalize)s.
//! - A recording that runs by itself, is placed on the timeline and has its end
//!   reported to `Session::update` is `Session::record`'s take, not this.

use thiserror::Error;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{InterleavedMut, Pull, Sample, Sink, SinkFailed, Source};
use tutti_units::Samples;

/// Why a [`PumpLoop`] could not be built.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PumpLoopError {
    /// The source and sink disagree on channel width.
    ///
    /// Returned by [`PumpLoop::new`] before any frame moves: recording a
    /// mismatched pair would rotate the file's channels every frame. It is the
    /// release-build half of the width-agreement invariant (the debug half is
    /// `pump`'s `debug_assert_eq!`), and it carries both layouts as values, not
    /// prose.
    #[error(
        "recorder source is {src:?} but the sink is {sink:?}; \
         recording a mismatched pair rotates the file's channels every frame"
    )]
    ChannelWidthMismatch {
        /// What the [`Source`] reports.
        src: ChannelLayout,
        /// What the [`Sink`] was opened with.
        sink: ChannelLayout,
    },
}

/// Why [`PumpLoop::finalize`] did not end cleanly.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FinalizeError<E: std::error::Error + 'static> {
    /// The source broke mid-take ([`Pull::Failed`]) after the sink closed
    /// cleanly: the file is valid and holds the frames delivered before the
    /// failure.
    #[error("the recording source failed; the take holds only what it delivered before")]
    SourceFailed,

    /// The sink's [`finalize`](Sink::finalize) failed.
    #[error("the recording sink failed to close")]
    Sink(#[source] E),
}

/// Moves one block from a [`Source`] to a [`Sink`]: the source's verdict, or
/// the sink's refusal.
///
/// Pulls as many frames as `buf` holds from `src` and writes exactly what it
/// produced to `dst`. The loop and the stop condition are the caller's policy
/// ([`PumpLoop`]). `buf` is interleaved and therefore
/// *sized* in samples ([`ChannelLayout::interleaved_len`]); everything returned is in
/// frames.
///
/// Behaviour, not vocabulary: its one caller is [`PumpLoop::pump_once`].
///
/// # Panics
///
/// In debug builds, if `src` and `dst` report different widths: the inner
/// half of the width check, whose outer half is [`PumpLoop::new`]'s error
/// (which a shipped binary relies on, so panicking mid-take is the wrong
/// response here). In any build, if `src` claims more frames than
/// `buf` holds, which breaks the [`Source::pull`] contract.
fn pump<I, O>(src: &mut I, dst: &mut O, buf: &mut [I::Sample]) -> Result<Pull, SinkFailed>
where
    I: Source + ?Sized,
    O: Sink<Sample = I::Sample> + ?Sized,
{
    debug_assert_eq!(
        src.layout(),
        dst.layout(),
        "pump source and sink must agree on channel width; \
         a mismatch rotates the sink's channels every frame"
    );
    // The scratch is sized whole frames of `src.layout()` by `PumpLoop::new`, so
    // the view builds; only a zero width does not, and a source of no channels
    // has nothing to pump.
    let Ok(mut out) = InterleavedMut::new(buf, src.layout()) else {
        return Ok(Pull::Failed);
    };
    let pull = src.pull(out.reborrow());
    if let Pull::Frames(n) = pull {
        dst.write(out.as_interleaved().slice(Samples::ZERO..n))?;
    }
    Ok(pull)
}

/// What one [`PumpLoop::pump_once`] pass achieved.
///
/// The "nothing moved" cases are separate because a driver must act
/// differently on each (park and re-pull, or stop), and the source's [`Pull`]
/// verdict is what tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpPass {
    /// Frames moved from the source into the sink. Never zero, since a
    /// zero-frame pass is one of the other variants.
    Wrote(Samples),
    /// The source had nothing ready *yet* ([`Pull::Starved`]). Park and come
    /// back; there is more coming.
    Starved,
    /// The source has nothing more, ever ([`Pull::Ended`]). Stop.
    Ended,
    /// The source broke ([`Pull::Failed`], which comes only after its buffered
    /// frames were drained), or a write to the sink failed ([`SinkFailed`]).
    /// Stop and finalize what was written.
    ///
    /// A finite source that failed is this, not [`Ended`](Self::Ended).
    Failed,
}

/// One source-to-sink pump, with no thread and no clock.
///
/// Owns both endpoints and a scratch buffer. [`pump_once`](Self::pump_once)
/// moves at most the capacity it was built with and allocates nothing. Step it
/// by hand to count frames exactly, or run it from a loop of the app's own.
///
/// The channel width is not a type parameter: the endpoints carry it as a
/// runtime [`ChannelLayout`].
///
/// # Examples
///
/// Recording what the graph is playing needs no device: a tap on the graph's
/// two outputs fills as the graph renders, the tap's reader is a [`Source`]
/// as it is, and one pass moves a block into a file. Swap the reader for
/// `tutti_cpal::MicIn` and the same lines record a microphone: that
/// interchangeability is the reason the traits exist.
///
/// ```
/// use tutti_codec::{FileOut, Format, Pcm, Spec};
/// use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, TransportPosition, Unforkable};
/// use tutti_session::{PumpLoop, PumpPass};
/// use tutti_graph::{OutPort, PortSource};
/// use tutti_units::{Beat, Bpm, SampleRate, Samples};
/// use tutti_buffers::{ChannelLayout, MaxBlock};
/// use tutti_buffers::Sink;
/// use tutti_graph::NodeKey;
/// use tutti_graph::{TapId, TapOptions};
///
/// /// A stereo source: 0.25 on the left, -0.25 on the right.
/// struct Tone;
/// impl Node for Tone {
///     fn shape(&self) -> Shape {
///         Shape::source_unassigned(ChannelLayout::STEREO)
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         io.output(0).fill(0.25);
///         io.output(1).fill(-0.25);
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
///
/// // A tap on both of the graph's outputs, copied as the graph renders.
/// let (mut editor, mut executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
/// editor.insert(NodeKey(1), "tone", Unforkable(Tone));
/// let out = |port| PortSource::Node(OutPort { node: NodeKey(1), port });
/// editor.spec_mut().topology.set_outputs([out(0), out(1)]);
/// let reader = editor
///     .open_tap(TapId::of(1), vec![out(0), out(1)], TapOptions::new(Samples::new(1024)))
///     .expect("the ports exist");
/// editor.commit().expect("compiles");
/// executor.apply_pending();
/// editor.collect();
///
/// // The audio callback renders three frames.
/// let (mut left, mut right) = ([0.0f32; 3], [0.0f32; 3]);
/// let transport = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
/// executor.process(3, &transport, &[], &mut [&mut left[..], &mut right[..]]);
///
/// let dir = tempfile::tempdir().expect("temp dir");
/// let spec = Spec::of_width(Format::Wav(Pcm::Float32), ChannelLayout::STEREO, SampleRate::SR_48K);
/// let file = std::fs::File::create(dir.path().join("take.wav")).expect("file opens");
/// let sink = FileOut::new(&spec, file).expect("sink opens");
///
/// // `new` checks the widths agree, before a frame moves.
/// let mut pump = PumpLoop::new(reader, sink, Samples::new(1024)).expect("stereo into stereo");
/// assert_eq!(pump.pump_once(), PumpPass::Wrote(Samples::new(3)));
/// // An empty tap is a producer that has not caught up, not the end.
/// assert_eq!(pump.pump_once(), PumpPass::Starved);
///
/// // `finalize` takes `self`, so the header back-patch happens exactly once and
/// // writing after it is a compile error rather than a corrupt file.
/// pump.finalize().expect("header back-patches");
/// ```
///
/// # Two things worth knowing
///
/// - **The source's verdict is not optional bookkeeping.** An empty pull
///   means "not yet" for a mic and "never again" for a decoded file, and only
///   the source's [`Pull`] distinguishes them. A source that answers
///   [`Ended`](Pull::Ended) where it means [`Starved`](Pull::Starved) ends a
///   take milliseconds in with no error anywhere (the recorder tests pin each
///   source's answer).
/// - **The width check is mandatory.** [`new`](Self::new) is the one place both
///   endpoints are in scope before a frame moves, so a width mismatch is an
///   error there rather than a file whose channels rotate every frame.
pub struct PumpLoop<I: Source, O> {
    src: I,
    sink: O,
    /// Allocated once at construction; `pump_once` never grows it.
    scratch: Vec<I::Sample>,
    /// Set when the source answered [`Pull::Failed`], so
    /// [`finalize`](Self::finalize) can say so after the sink closed.
    source_failed: bool,
    /// Set by the first [`PumpPass::Failed`].
    failed: bool,
    /// Frames the source discarded, summed ([`Source::take_lost`]).
    lost: Samples,
}

impl<I: Source, O> std::fmt::Debug for PumpLoop<I, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpLoop")
            .field("scratch_len", &self.scratch.len())
            .finish_non_exhaustive()
    }
}

impl<I, O> PumpLoop<I, O>
where
    I: Source,
    O: Sink<Sample = I::Sample>,
{
    /// A loop over `src` and `sink` moving at most `capacity` frames a pass.
    ///
    /// `capacity` is in **frames**, a [`Samples`], so a caller cannot hand over
    /// a sample count by mistake and get a buffer `width` times too large. The
    /// scratch is allocated once, sized
    /// [`layout.interleaved_len(capacity)`](ChannelLayout::interleaved_len) from the
    /// width both endpoints agreed on. A zero capacity is taken as one frame:
    /// an empty scratch would make every poll return zero, ending a finite
    /// take before its first frame and parking a live one forever.
    ///
    /// The **sample rate** is the caller's to match: [`Source`] carries none,
    /// so nothing here can compare them.
    ///
    /// # Errors
    ///
    /// [`PumpLoopError::ChannelWidthMismatch`] when `src` and `sink` disagree
    /// on channel width, before any frame is written.
    pub fn new(src: I, sink: O, capacity: Samples) -> Result<Self, PumpLoopError> {
        let layout = src.layout();
        if layout != sink.layout() {
            return Err(PumpLoopError::ChannelWidthMismatch {
                src: layout,
                sink: sink.layout(),
            });
        }
        let frames = if capacity.is_zero() {
            Samples::new(1)
        } else {
            capacity
        };
        Ok(Self {
            src,
            sink,
            scratch: vec![I::Sample::SILENCE; layout.interleaved_len(frames)],
            source_failed: false,
            failed: false,
            lost: Samples::ZERO,
        })
    }

    /// Runs one pass: pulls a block of frames from the source and writes them
    /// to the sink.
    ///
    /// An empty pull is reported as [`Starved`](PumpPass::Starved),
    /// [`Ended`](PumpPass::Ended) or [`Failed`](PumpPass::Failed) as the source's
    /// own [`Pull`] says. A write the sink refused is
    /// [`Failed`](PumpPass::Failed) too.
    pub fn pump_once(&mut self) -> PumpPass {
        let pass = match pump(&mut self.src, &mut self.sink, &mut self.scratch) {
            Err(SinkFailed) => PumpPass::Failed,
            Ok(Pull::Frames(n)) => PumpPass::Wrote(n),
            Ok(Pull::Starved) => PumpPass::Starved,
            Ok(Pull::Ended) => PumpPass::Ended,
            Ok(Pull::Failed) => {
                self.source_failed = true;
                PumpPass::Failed
            }
        };
        self.lost += self.src.take_lost();
        self.failed |= pass == PumpPass::Failed;
        pass
    }

    /// Whether a pass has failed: the source broke or the sink did.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The frames the source discarded so far ([`Source::take_lost`]), summed
    /// over the passes run. A recording closes up over each gap.
    #[must_use]
    pub fn lost(&self) -> Samples {
        self.lost
    }

    /// Consumes the loop and closes the sink (for a file, back-patching its
    /// header).
    ///
    /// Taking `self` makes it once-only: a loop that has finalized cannot pump
    /// again.
    ///
    /// # Errors
    ///
    /// [`FinalizeError::Sink`] with the sink's [`finalize`](Sink::finalize)
    /// error; for a WAV, the file is then unreadable. If the sink closed
    /// cleanly but the source had failed, [`FinalizeError::SourceFailed`]: the
    /// file is valid and holds the frames delivered before the failure.
    pub fn finalize(self) -> Result<(), FinalizeError<O::Error>> {
        let source_failed = self.source_failed;
        self.sink.finalize().map_err(FinalizeError::Sink)?;
        if source_failed {
            return Err(FinalizeError::SourceFailed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
