//! A stream's scalar cells: the controls and counts the audio thread and the
//! streamer share without a lock.
//!
//! One [`Cells`] per open stream (a re-open is a new stream with new cells), held
//! by the streamer's `Record`, the stream's handle and its one voice. Nothing a
//! second voice can reach: two voices opened in sequence on one file share no
//! cell. Every field is a scalar atomic and has
//! one writer:
//!
//! | Field | Writer | Readers |
//! |---|---|---|
//! | `speed`, `stretch_rate` | the voice's command drain (`DiskVoice::set_speed`, the slot's stretch step), or the control thread through `DiskVoiceControls`; the audio thread cannot publish, so these stay scalars | the voice's per-sample step; the streamer, to size a fill (`read_rate`) |
//! | `gain` | the voice's command drain, or the control thread through `DiskVoiceControls` | the voice |
//! | `src_ratio` | the streamer alone, derived from the session rate when a stream opens and on a rate event | the voice, until `prepare` tells it a rate |
//! | `underruns` | the voice (audio thread) | the stream's handle, which drains it |
//!
//! What is not here: the direction and the applied loop are the ring's
//! published `RingControl` (the streamer is their one writer, when it takes
//! the loop or direction the stream's control side publishes), and
//! the ring's occupancy is `refill_one`'s return, not a shared cell.
//!
//! Where a stream stands and how to read it is the ring's, not this cell's
//! (`prefetch::Ring`): the reader publishes the position it plays and the
//! streamer follows it, so there is nothing to request and nothing to flush.

use atomic_float::AtomicF64;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_rt::Param64;
use tutti_units::{Amplitude, PlaybackRate, ReadRate, SrcRatio};

/// A stream's scalar controls and counts: see the module docs for who writes
/// each.
///
/// Cache-line aligned in two groups, so the streamer's reads of the controls do
/// not false-share with the audio thread's per-block write of the underrun
/// count.
#[derive(Debug)]
pub struct Cells {
    /// Varispeed, conversion ratio, stretch and gain.
    playback: PlaybackParams,
    /// Underruns the voice counted since the handle last drained them.
    health: BufferHealth,
}

#[repr(align(64))]
#[derive(Debug)]
struct PlaybackParams {
    speed: Param64<PlaybackRate>,
    /// file_sample_rate / session_sample_rate. 1.0 = no conversion.
    ///
    /// A [`Param64`]: [`SrcRatio`] is `f64`, so the cell keeps the bits a
    /// stream's position accumulates against.
    src_ratio: Param64<SrcRatio>,
    /// Linear output gain, an [`Amplitude`] in the cell.
    ///
    /// A cell rather than a field of `DiskVoice` for the reason `tutti_nodes`'
    /// crate docs give: a live control is a shared cell the control thread
    /// writes through `&self` (`DiskVoiceControls::set_gain`), never a field of
    /// the node.
    gain: AtomicF64,
    /// Source samples consumed per output sample by a wrapping time-stretcher:
    /// `1 / stretch`. 1.0 when the voice does not stretch.
    ///
    /// Held here rather than in `DiskVoice` because the stretch vocoder and the
    /// ring reader never meet: the vocoder lives in the `PlaybackSlot`, the reader
    /// behind the streamer's `SharedReader`. This is the cell both already
    /// share, and it is where the other two rate factors compose.
    ///
    /// A [`Param64`]: [`ReadRate`] is `f64`, and why the width matters is stated
    /// on [`Param64`].
    stretch_rate: Param64<ReadRate>,
}

#[repr(align(64))]
#[derive(Debug)]
struct BufferHealth {
    /// Underruns since the last [`Cells::take_underruns`]. Audio thread
    /// increments, the stream's handle drains.
    underrun_count: AtomicU64,
}

impl Default for Cells {
    fn default() -> Self {
        Self::new()
    }
}

impl Cells {
    /// Cells at rest: unity speed, conversion ratio and gain, no stretch, no
    /// underruns.
    pub fn new() -> Self {
        Self {
            playback: PlaybackParams {
                speed: Param64::new(PlaybackRate::UNITY),
                src_ratio: Param64::new(SrcRatio::UNITY),
                gain: AtomicF64::new(1.0),
                stretch_rate: Param64::new(ReadRate::UNITY),
            },
            health: BufferHealth {
                underrun_count: AtomicU64::new(0),
            },
        }
    }

    /// New cells holding these cells' playback controls (speed, gain,
    /// conversion ratio, stretch rate) at their current values, and nothing
    /// else: no counters.
    ///
    /// What a disk voice forked for an offline render keeps
    /// (`DiskVoice::fork_copy`): its controls as a snapshot, like every other
    /// forked unit's, in cells no streamer and no live voice shares.
    pub(crate) fn detached(&self) -> Self {
        let cells = Self::new();
        cells.set_speed(self.speed());
        cells.set_gain(self.gain());
        cells.set_src_ratio(self.src_ratio());
        cells.set_stretch_rate(self.stretch_rate());
        cells
    }

    /// The current varispeed. [`PlaybackRate::UNITY`] is normal speed.
    #[inline]
    pub fn speed(&self) -> PlaybackRate {
        self.playback.speed.load()
    }

    /// Publish a new varispeed.
    ///
    /// Takes the already-bounded [`PlaybackRate`] rather than a raw `f64` so the
    /// range is enforced where the value is *built*. A clamp applied here
    /// instead would bind only this tier — the in-memory sampler never calls
    /// this setter, so the same command would produce different audio depending
    /// on which tier happened to be playing.
    pub fn set_speed(&self, speed: PlaybackRate) {
        self.playback.speed.store(speed);
    }

    /// The voice's linear output gain.
    #[inline]
    pub fn gain(&self) -> Amplitude {
        Amplitude::new(self.playback.gain.load(Ordering::Acquire))
    }

    /// Publish a new output gain.
    ///
    /// `&self`, like every other publisher here: the whole point of this cell
    /// is that the control thread and the audio thread hold the *same* one, so
    /// no `&mut` is available or needed.
    pub fn set_gain(&self, gain: Amplitude) {
        self.playback.gain.store(gain.get(), Ordering::Release);
    }

    /// The varispeed as the audio thread's per-sample path names it. Identical
    /// to [`speed`](Self::speed); the separate name marks the hot call site
    /// rather than the raw atomic load.
    #[inline]
    pub fn effective_speed(&self) -> PlaybackRate {
        self.speed()
    }

    /// Source samples consumed per output sample: varispeed × conversion ×
    /// stretch.
    ///
    /// The streaming twin of `MemorySource::read_rate`, composing through the
    /// same [`PlaybackRate::read_rate`] so neither tier can drop a factor or
    /// swap the pair.
    ///
    /// The stretch term is folded in here, the one composition point: the
    /// per-sample advance and the `samples_needed` fetch estimate all read this
    /// rate and must agree with it or the ring under- or over-runs. Without
    /// the term a stretched disk voice consumes as many source frames as an
    /// unstretched one, and the vocoder has nothing to spread.
    #[inline]
    pub fn read_rate(&self) -> ReadRate {
        self.speed()
            .read_rate(self.src_ratio())
            .then(self.stretch_rate())
    }

    /// The wrapping stretcher's read rate — `1 / stretch`, or unity when the
    /// voice does not stretch.
    #[inline]
    pub fn stretch_rate(&self) -> ReadRate {
        self.playback.stretch_rate.load()
    }

    /// Publish the wrapping stretcher's read rate. Control thread, or the audio
    /// thread's own parameter application — a single relaxed store either way.
    #[inline]
    pub fn set_stretch_rate(&self, rate: ReadRate) {
        self.playback.stretch_rate.store(rate);
    }

    /// Sample-rate conversion ratio: file rate over session rate.
    /// [`SrcRatio::UNITY`] means the file already plays at the session rate.
    #[inline]
    pub fn src_ratio(&self) -> SrcRatio {
        self.playback.src_ratio.load()
    }

    /// Publish the conversion ratio. Written by the streamer alone: when a stream starts,
    /// and when the session rate moves, from [`SrcRatio::for_rates`]
    /// — the same derivation the in-memory tier uses, so neither tier can pick
    /// the ratio up backwards.
    pub fn set_src_ratio(&self, ratio: SrcRatio) {
        self.playback.src_ratio.store(ratio);
    }

    /// Count one frame the ring could not supply. Audio side: a single relaxed
    /// increment, allocation-free and safe on the hot path.
    #[inline]
    pub fn report_underrun(&self) {
        self.health.underrun_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Read and zero the underrun count, returning the frames missed since the
    /// previous call. Draining rather than peeking is what makes the figure a
    /// per-interval rate instead of an ever-growing total.
    pub fn take_underruns(&self) -> u64 {
        self.health.underrun_count.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stretch rate survives the cell bit-for-bit: the term a disk voice
    /// accumulates into its read position once per sample, where an error in
    /// the stored rate integrates.
    #[test]
    fn the_stretch_rate_cell_returns_what_it_is_given() {
        let state = Cells::new();
        // 1/3, the ratio a triplet stretch produces.
        let rate = ReadRate::new(1.0 / 3.0);
        state.set_stretch_rate(rate);
        assert_eq!(state.stretch_rate(), rate);
    }

    #[test]
    fn test_speed_clamping() {
        // The clamp lives in `PlaybackRate`, not in this setter, so BOTH
        // playback tiers get it — the in-memory sampler never calls this
        // setter, and a clamp here would leave it accepting out-of-range speeds.
        let state = Cells::new();

        // A fresh cell reads UNITY, not the atomic's zero — which would be
        // silence rather than normal speed, and is what `PlaybackParams`'
        // derived `Default` would give if it stopped spelling this out.
        assert_eq!(state.speed(), PlaybackRate::UNITY);

        state.set_speed(PlaybackRate::clamped(0.1));
        assert_eq!(state.speed(), PlaybackRate::MIN);

        state.set_speed(PlaybackRate::clamped(10.0));
        assert_eq!(state.speed(), PlaybackRate::MAX);

        state.set_speed(PlaybackRate::clamped(2.0));
        assert_eq!(state.speed(), PlaybackRate::of(2.0));
    }

    #[test]
    fn test_underrun_reporting() {
        let state = Cells::new();

        state.report_underrun();
        state.report_underrun();
        state.report_underrun();
        assert_eq!(state.take_underruns(), 3);
        assert_eq!(state.take_underruns(), 0);

        state.report_underrun();
        assert_eq!(state.take_underruns(), 1);
    }
}
