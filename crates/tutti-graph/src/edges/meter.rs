//! Level meters: declared readers on ports of the compiled plan.
//!
//! A meter is declared on the [`Topology`](crate::Topology) as "measure
//! these sources" ([`MeterSpec`](crate::MeterSpec)): the main outputs
//! for a main output meter (`Outputs`, aligned: each lane is what its output
//! channel carries, at the output's time), any node's output ports for a track
//! (`Ports { sources, align }`, as they leave their nodes or aligned to the
//! output time). The compiler emits one [`Op::Meter`](crate::Op::Meter) per
//! meter, which reads every lane's source once per block and folds it into
//! that lane's cell. A lane whose port does not exist reads silence. A meter
//! that is not in the topology compiles to nothing, and a meter never changes
//! an output sample.
//!
//! # What a lane holds
//!
//! A lane is a [`tutti_rt::level_cell`]: the peak, the frame count and the
//! energy (a sum of squares; both readings `f64`, so an hour unread stays
//! exact) since the reader last read. A block's figures accumulate in the
//! graph's float over chunks of at most 256 frames, each chunk folded into
//! `f64`: the rounding bound does not depend on the block's length, and no
//! sample narrows before it is measured. The op folds each block into it and a read takes it
//! whole, so **a block lands in one read, peak and energy together**: a read
//! with no frames has no peak and no energy. The cell is `tutti-rt`'s, with a
//! loom model; the two ends are wait-free for the op and wait for a block in
//! flight on the reader's side.
//!
//! The audio side keeps no state of its own and no clock: **ballistics**
//! (decay, peak-hold timers, dB scaling) belong to the reader, which knows
//! its own refresh rate. An offline render runs faster than any clock, so a
//! decay on the audio side would mean nothing there.
//!
//! # One consumer
//!
//! [`MeterReader::read`] takes what was held, so two consumers of one reader
//! would steal each other's peaks. The reader is therefore not `Clone` and
//! reads through `&mut self`; a second consumer declares a second meter.
//!
//! # Parallel safety
//!
//! The op writes only the cells' atomics, and a lane has exactly one writer by
//! construction (lane `i` is source `i` of the meter's one op), so the cells
//! need no claim: the one-participant and parallel executors run the same
//! [`meter_op`] and leave the same bits.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use crate::MeterId;
use tutti_rt::{ControlDrop, LevelReader, LevelSpan, LevelWriter, level_cell};
use tutti_units::{Amplitude, Samples};

use super::alive::Alive;
use crate::compile::ids::AudioSlotId;
use crate::exec::SILENT;
use crate::exec::slots::{AudioSlots, SlotFlags};
use tutti_buffers::Float;

/// What one lane measured since it was last read.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Level {
    /// The largest absolute sample.
    pub peak: Amplitude,
    /// The root-mean-square over [`frames`](Self::frames). A non-finite sample
    /// makes it non-finite for that read (a peak ignores NaN).
    pub rms: Amplitude,
    /// How many frames this covers. Zero: no block was measured since the
    /// last read (the meter is off, or the reader is faster than the blocks),
    /// and the figures above are silence, not a measurement.
    pub frames: Samples,
}

impl From<LevelSpan> for Level {
    fn from(span: LevelSpan) -> Self {
        let rms = if span.frames == 0 {
            0.0
        } else {
            (span.sum_sq / span.frames as f64).sqrt()
        };
        Self {
            peak: Amplitude::new(span.peak),
            rms: Amplitude::new(rms),
            frames: Samples::new(usize::try_from(span.frames).unwrap_or(usize::MAX)),
        }
    }
}

/// What the two ends share besides the lanes.
#[derive(Debug)]
struct Shared {
    /// The lanes the current plan writes (at most the lanes there are).
    width: AtomicU32,
    enabled: AtomicBool,
}

/// One block's figures for one lane: the readings, in `f64`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BlockStats {
    peak: f64,
    sum_sq: f64,
}

/// Samples a pass folds at once. The partial sums are a fixed shape, so the
/// result is the same on every executor and the loop vectorizes (a plain
/// left-to-right sum cannot).
const WIDTH: usize = 8;

/// Frames one accumulation covers in the graph's float before it folds into an
/// `f64`: a partial sum holds at most `CHUNK / WIDTH` terms, so its rounding
/// bound is the same for a 64-frame block and a 1 Mi-frame one.
const CHUNK: usize = 256;

/// Peak and sum of squares of `block`, accumulated in `F` over chunks of at
/// most [`CHUNK`] frames and each chunk folded into `f64`. NaN is ignored by
/// the peak and poisons the sum.
#[inline]
fn block_stats<F: Float>(block: &[F]) -> BlockStats {
    let mut stats = BlockStats::default();
    for chunk in block.chunks(CHUNK) {
        let (peak, sum_sq) = chunk_stats(chunk);
        stats.peak = stats.peak.max(peak.to_f64());
        stats.sum_sq += sum_sq.to_f64();
    }
    stats
}

/// Peak and sum of squares of one chunk in `F`: `WIDTH` running figures per
/// pass, combined in lane order at the end, then the tail in order.
#[inline]
fn chunk_stats<F: Float>(chunk: &[F]) -> (F, F) {
    let (passes, tail) = chunk.as_chunks::<WIDTH>();
    let mut peak = [F::ZERO; WIDTH];
    let mut sum = [F::ZERO; WIDTH];
    for pass in passes {
        for i in 0..WIDTH {
            peak[i] = peak[i].max(pass[i].abs());
            sum[i] += pass[i] * pass[i];
        }
    }
    let mut peak = peak.iter().fold(F::ZERO, |a, &p| a.max(p));
    let mut sum_sq = sum.iter().fold(F::ZERO, |a, &x| a + x);
    for &s in tail {
        peak = peak.max(s.abs());
        sum_sq += s * s;
    }
    (peak, sum_sq)
}

/// The executor's end of a meter: a writer per lane and the switch. Not
/// `Clone`; one per meter.
#[derive(Debug)]
pub struct MeterWriter {
    _control: ControlDrop<Self>,
    lanes: Box<[LevelWriter]>,
    shared: Arc<Shared>,
}

/// [`Op::Meter`](crate::Op::Meter): the first `frames` samples of each of
/// `srcs` into the meter's lanes, if the reader switched it on. A slot the
/// executor knows is silent is not scanned: it adds its frames and no energy.
#[inline]
pub(crate) fn meter_op<F: Float, A: AudioSlots<F> + ?Sized, Fl: SlotFlags + ?Sized>(
    meter: &mut MeterWriter,
    arena: &A,
    flags: &Fl,
    srcs: &[AudioSlotId],
    frames: usize,
) {
    if !meter.shared.enabled.load(Ordering::Relaxed) {
        return;
    }
    // A block is at most `MaxBlock` frames, far below `u32::MAX`; a longer one
    // (unreachable) is not counted.
    let Ok(n) = u32::try_from(frames) else {
        return;
    };
    for (lane, &src) in meter.lanes.iter_mut().zip(srcs) {
        let stats = if flags.get(src) & SILENT != 0 {
            BlockStats::default()
        } else {
            block_stats(arena.slot(src, frames))
        };
        lane.fold(stats.peak, stats.sum_sq, n);
    }
}

impl MeterWriter {
    /// Records how many lanes the plan being applied writes. Control or
    /// audio thread: one store.
    pub(crate) fn set_width(&self, lanes: usize) {
        let lanes = lanes.min(self.lanes.len());
        self.shared
            .width
            .store(u32::try_from(lanes).unwrap_or(u32::MAX), Ordering::Relaxed);
    }
}

/// The consumer's end of a meter: the level of each lane since the last read.
///
/// Not `Clone`, and reads take `&mut self`: one consumer per meter (see the
/// module docs). Switched **off** until [`enable`](Self::enable): the
/// op then skips its pass entirely. Dropping it leaves the meter in the plan,
/// measuring nothing anyone reads, until the editor closes or prunes it
/// ([`Editor::close_meter`](crate::Editor::close_meter),
/// [`Editor::prune_meters`](crate::Editor::prune_meters)). It must not be
/// dropped on the audio thread in a debug build.
#[derive(Debug)]
pub struct MeterReader {
    _control: ControlDrop<Self>,
    id: MeterId,
    lanes: Box<[LevelReader]>,
    shared: Arc<Shared>,
    /// The editor watches this: when it dies, the reader is gone.
    alive: Alive,
}

impl MeterReader {
    /// A reader and the writer the executor will fill it with, for `lanes`
    /// channels.
    pub(crate) fn pair(id: MeterId, lanes: usize, width: usize) -> (Self, MeterWriter) {
        let shared = Arc::new(Shared {
            width: AtomicU32::new(u32::try_from(width).unwrap_or(u32::MAX)),
            enabled: AtomicBool::new(false),
        });
        let (writers, readers): (Vec<_>, Vec<_>) = (0..lanes).map(|_| level_cell()).unzip();
        (
            Self {
                _control: ControlDrop::new(),
                id,
                lanes: readers.into(),
                shared: Arc::clone(&shared),
                alive: Alive::default(),
            },
            MeterWriter {
                _control: ControlDrop::new(),
                lanes: writers.into(),
                shared,
            },
        )
    }

    /// A handle to see whether this reader is still alive: its strong count
    /// is zero once the reader is dropped.
    #[must_use]
    pub fn watch(&self) -> Weak<()> {
        self.alive.watch()
    }

    /// The meter's id, the one [`Editor::close_meter`](crate::Editor::close_meter) takes.
    #[must_use]
    pub fn id(&self) -> MeterId {
        self.id
    }

    /// How many channels the meter measures now: its sources' count, as of
    /// the plan the executor runs (zero before the first commit that names
    /// it is applied).
    #[must_use]
    pub fn channels(&self) -> usize {
        self.shared.width.load(Ordering::Relaxed) as usize
    }

    /// Starts measuring: the op begins on the next block.
    pub fn enable(&self) {
        self.shared.enabled.store(true, Ordering::Relaxed);
    }

    /// Stops measuring, skipping the pass entirely. What was held stays
    /// readable.
    pub fn disable(&self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
    }

    /// Whether the op is filling this meter.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Relaxed)
    }

    /// Whether the meter's writer is gone: the meter was closed and the
    /// plan that ran it retired, so nothing will be measured again.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }

    /// Takes what each channel measured since the last read into `out`, one
    /// [`Level`] per channel, and returns how many it wrote (the fewer of
    /// [`channels`](Self::channels) and `out.len()`). Takes only the lanes it
    /// writes. Allocates nothing; any thread that owns the reader.
    pub fn read_into(&mut self, out: &mut [Level]) -> usize {
        let width = self.channels().min(self.lanes.len());
        let mut n = 0;
        for (slot, lane) in out.iter_mut().zip(&mut self.lanes[..width]) {
            *slot = lane.take().into();
            n += 1;
        }
        n
    }

    /// As [`read_into`](Self::read_into), into a new `Vec`.
    #[must_use]
    pub fn read(&mut self) -> Vec<Level> {
        let mut out = vec![Level::default(); self.channels()];
        let n = self.read_into(&mut out);
        out.truncate(n);
        out
    }
}

#[cfg(test)]
mod tests;
