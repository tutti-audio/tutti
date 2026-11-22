//! Live analysis: an [`Analyzer`] is a tap with a meter's state.
//!
//! [`Taps::open_analyzer`](crate::Taps::open_analyzer) opens a tap on the graph and binds
//! the analysis to the graph's rate and the tap's width, through the same
//! [`AudioContext`] and [`AudioInput`]
//! the offline [`Session::analyze`](crate::Session::analyze) binds a wave
//! with. The app drains it on a thread of its own ([`Analyzer::pending`]); the
//! session starts none.

use tutti_buffers::{ChannelLayout, ChannelTopology, Float, Pull, Source};
use tutti_graph::{MissingSource, TapReader};
use tutti_measure::{Measure, StreamMeter};
use tutti_units::{SampleRate, Samples};

use super::feed::{Bind, Chunk, View};
use super::{AudioContext, AudioInput, MeterBindError};

/// Frames shown to the meter at a time.
const CHUNK_FRAMES: Samples = Samples::new(16_384);

/// Why an [`Analyzer`] was not opened. Nothing is opened on any of them.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AnalyzerError {
    /// A source is not in the graph.
    #[error(transparent)]
    MissingSource(#[from] MissingSource),
    /// The analysis cannot be bound to the graph's rate and the tap's width
    /// (a band above the Nyquist rate, say).
    #[error("the analysis refused the graph's rate and the tap's width: {0}")]
    Measure(#[from] tutti_measure::MeterError),
    /// The tap's channels cannot be shown to the meter in the view it takes:
    /// a stereo analysis on a tap of another width, or a mono analysis with
    /// no fold from the tap's width.
    #[error("the tap's channels cannot be shown to the analysis: {0}")]
    Bind(#[from] MeterBindError),
}

/// A tap with a meter's state: the app drains it with
/// [`pending`](Self::pending) and reads what the tap lost
/// ([`lost`](Self::lost)). Opened by [`Taps::open_analyzer`](crate::Taps::open_analyzer).
///
/// Generic over the analysis `A`, never boxed: [`StreamMeter`] is not object-safe.
/// Drain it from a thread of your own, never the audio thread. Dropping it
/// leaves the tap open until the session's next
/// [`Reclaim`](crate::Phase::Reclaim) phase, which closes it. It is read on
/// one thread: [`lost`](Self::lost) keeps its total in a `Cell`, so an
/// `Analyzer` is `Send` and not `Sync`.
pub struct Analyzer<A: Measure<F>, F: Float = f32> {
    reader: TapReader<F>,
    meter: A::Meter,
    chunk: Chunk<'static, F>,
    /// The items the last drain collected; handed out by `pending`.
    items: Vec<<A::Meter as StreamMeter>::Item>,
    /// The tap's losses read so far; `lost` adds what the tap counted since.
    lost: std::cell::Cell<Samples>,
    refused: Samples,
    ended: bool,
    failed: bool,
}

impl<A: Measure<F>, F: Float> std::fmt::Debug for Analyzer<A, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analyzer")
            .field("reader", &self.reader)
            .field("lost", &self.lost.get())
            .field("refused", &self.refused)
            .field("ended", &self.ended)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl<A, F> Analyzer<A, F>
where
    F: Float,
    A: Measure<F>,
    A::Context: AudioContext,
    for<'a> <A::Meter as StreamMeter>::Input<'a>: AudioInput<'a, F>,
{
    /// Opens the tap `open` makes, once `analysis` is bound to `rate` and to
    /// `width` channels and its view of them is known to exist.
    pub(crate) fn open(
        rate: SampleRate,
        width: u16,
        analysis: &A,
        open: impl FnOnce() -> Result<TapReader<F>, MissingSource>,
    ) -> Result<Self, AnalyzerError> {
        let layout = ChannelLayout::from_count(width);
        let meter = analysis.measure(A::Context::from_audio(rate, layout))?;
        let mut chunk = Chunk::new(&ChannelTopology::default_for(layout));
        // An empty chunk shows whether the meter's view of this width exists.
        chunk.fill(Samples::ZERO)?;
        <<A::Meter as StreamMeter>::Input<'_> as View<'_, F>>::view(&mut chunk)?;
        let reader = open()?;
        Ok(Self {
            reader,
            meter,
            chunk,
            items: Vec::new(),
            lost: std::cell::Cell::new(Samples::ZERO),
            refused: Samples::ZERO,
            ended: false,
            failed: false,
        })
    }

    /// Shows the meter what the tap holds now and yields the items it
    /// completed, in order. Never blocks: an empty tap yields nothing.
    ///
    /// A chunk the meter refuses (a NaN or an infinity, which a node may pass
    /// on) is skipped and counted ([`refused`](Self::refused)); the chunks
    /// after it are shown as usual. Once the tap has ended
    /// ([`is_finished`](Self::is_finished)) or failed ([`is_failed`](Self::is_failed)) there is nothing more.
    pub fn pending(&mut self) -> impl Iterator<Item = <A::Meter as StreamMeter>::Item> + '_ {
        self.drain();
        self.items.drain(..)
    }

    /// A future that resolves once the tap has written a block or ended since
    /// the last time one resolved ([`TapReader::notified`](tutti_graph::TapReader::notified)):
    /// await it, then drain with [`pending`](Self::pending). `None` for a tap
    /// opened without bells, which a session's never is.
    ///
    /// The audio thread rings the tap's bell as it writes a block, and the
    /// session's bell task on its executor wakes the future. On the web, where
    /// the executor starts no thread for it, [`Session::update`](crate::Session::update)
    /// runs that pass.
    pub fn notified(&mut self) -> Option<impl Future<Output = ()> + '_> {
        self.reader.notified()
    }

    /// Drains what is left and answers over the whole stream. The items this
    /// last drain completes are dropped: drain with [`pending`](Self::pending)
    /// first to keep them.
    #[must_use]
    pub fn finish(mut self) -> <A::Meter as StreamMeter>::Output {
        self.drain();
        self.meter.finish()
    }

    /// Pops the tap empty into the meter, a chunk at a time.
    fn drain(&mut self) {
        while !self.ended && !self.failed {
            let frames = self.reader.available().min(CHUNK_FRAMES);
            // The chunk holds `CHUNK_FRAMES` of the tap's width, so a fill of at
            // most that many frames is in range; one that is not is a fault of
            // the analyzer, reported as one.
            let Ok(buffer) = self.chunk.fill(frames) else {
                self.failed = true;
                return;
            };
            match self.reader.pull(buffer) {
                Pull::Frames(read) => self.chunk.truncate(read),
                Pull::Ended => self.ended = true,
                Pull::Failed => self.failed = true,
                Pull::Starved => return,
            }
            if !self.chunk.is_empty() {
                self.push();
            }
        }
    }

    /// Shows the meter the chunk, and counts its frames refused when it is not
    /// taken.
    fn push(&mut self) {
        let frames = self.chunk.frames();
        let taken = <<A::Meter as StreamMeter>::Input<'_> as View<'_, F>>::view(&mut self.chunk)
            .ok()
            .is_some_and(|view| self.meter.push(view, &mut self.items).is_ok());
        if !taken {
            self.refused += frames;
        }
    }
}

impl<A: Measure<F>, F: Float> Analyzer<A, F> {
    /// The frames the tap lost since it opened: what the app did not drain
    /// before the tap's ring (the capacity of the [`TapOptions`](tutti_graph::TapOptions) it opened with) was full.
    #[must_use]
    pub fn lost(&self) -> Samples {
        let total = self.lost.get() + self.reader.take_lost();
        self.lost.set(total);
        total
    }

    /// The frames of the chunks the meter refused, which the analysis never saw.
    #[must_use]
    pub fn refused(&self) -> Samples {
        self.refused
    }

    /// Whether the tap has ended: every source was removed and what the tap
    /// held was drained. Read after a [`pending`](Self::pending).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.ended
    }

    /// Whether the tap failed: a pull the tap refused (a chunk of another
    /// width than the tap's) or a chunk out of range. It is a fault, not an
    /// end: the frames before it were shown, and nothing more is read. The
    /// chunk is built at the tap's width when it opens, so no input of the
    /// app's reaches it; it is reported, never folded into
    /// [`is_finished`](Self::is_finished).
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed
    }
}
