//! A stretching voice's read: a [`PhaseVocoder`] fed from a source at a cursor.
//!
//! The vocoder is pulled by its output. For `m` output frames it says how many
//! input frames it consumes ([`PhaseVocoder::input_needed`]); the voice reads
//! exactly that many source frames at its cursor, one source step apart, and
//! calls [`process`](PhaseVocoder::process). The stretch never enters the read
//! step: the vocoder consumes `1 / stretch` input frames per output frame by
//! itself, and a pitch shift consumes none extra.
//!
//! # Seating
//!
//! The cursor is seated where the transport puts the voice at its first
//! sounding frame (the gate's position at the stretch's rate), and follows the
//! source from there. A change of the stretch or the pitch moves nothing: the
//! cursor and the vocoder's phase history carry on. The voice unseats, and
//! the vocoder resets, where the transport leaves the voice's window or
//! stands, and at a jump (a seek or a loop wrap); the next sounding frame
//! seats again.
//!
//! A memory voice's seat is primed and read ahead: the vocoder runs its [`latency`]
//! and a window more of output frames from source starting a window before the seat
//! and discards them, so the first frame of the block sounds the source at the
//! seat, on time, and what a voice that had been playing all along would (the
//! window is the ramp a stretched stream takes to fill). The voice's output is
//! then not late, and it declares no latency. A disk voice's ring reads forward and
//! holds nothing before a seat, so it is not primed: its first [`latency`]
//! frames are silence and it sounds [`latency`] frames late, which a standalone
//! node declares and a pool cannot.
//!
//! The cursor is an origin and the frames taken since: a position is the origin
//! plus the step times all of them, one product, so the frames a source is read at
//! do not depend on how the output was split into blocks.
//!
//! [`latency`]: PhaseVocoder::latency

use tutti_buffers::{ChannelLayout, Planes, PlanesMut};
use tutti_dsp::{
    ColaGeometry, HopSize, PhaseVocoder, PhaseVocoderControls, StftGeometry, WindowLength,
};
use tutti_units::{ReadRate, SamplePosition, SampleRate, Samples, StretchFactor};

use super::disk_voice::DiskVoice;
use super::memory_source::MemorySource;
use super::types::{Direction, Playback};
use crate::SamplerFloat;
use crate::lanes::{Gather, Lanes, scale};

/// The vocoder's analysis window, in frames: about 43 ms at 48 kHz.
const WINDOW: usize = 2048;

/// The vocoder's hop: a quarter of the window, the overlap Hann squared needs
/// to sum to a constant.
const HOP: usize = WINDOW / 4;

/// The first frame of a wave: before it a read gives silence.
const WAVE_START: SamplePosition = SamplePosition::new(0.0);

/// A phase vocoder over `width` channels on the voice's fixed geometry (a
/// 2048-frame Hann window, hop 512). Allocates: control thread only.
///
/// `rate` names the geometry's sample rate; the vocoder's arithmetic counts
/// frames and never reads it.
pub(crate) fn vocoder<F: SamplerFloat>(
    width: ChannelLayout,
    rate: SampleRate,
) -> Box<PhaseVocoder<F>> {
    let window = WindowLength::new(Samples::new(WINDOW)).expect("BUG: a nonzero window");
    let hop = HopSize::new(Samples::new(HOP)).expect("BUG: a nonzero hop");
    let geometry = ColaGeometry::try_from(StftGeometry::new(rate, window, hop))
        .expect("BUG: a hop of a quarter window is COLA-valid");
    Box::new(PhaseVocoder::new(width, geometry))
}

/// The controls a voice's intent asks of its vocoder.
pub(crate) fn controls(play: &Playback) -> PhaseVocoderControls {
    PhaseVocoderControls {
        stretch: play.stretch,
        pitch: play.pitch,
    }
}

/// Source frames a stretching voice consumes per output frame: `1 / stretch`,
/// the figure the transport's gate and a disk stream's fill sizing count by. A
/// non-finite stretch reads as unity and the rest is held to the supported
/// range, as the vocoder holds it.
pub(crate) fn consumption(stretch: StretchFactor) -> ReadRate {
    let s = stretch.get();
    let s = if s.is_finite() {
        s.clamp(StretchFactor::MIN.get(), StretchFactor::MAX.get())
    } else {
        1.0
    };
    ReadRate::new(1.0 / s)
}

/// Where the vocoder's next input frame is read in the source.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor {
    /// Where the frame `taken` frames ago was read, before the step last changed.
    origin: SamplePosition,
    /// Source frames per input frame taken since `origin`.
    step: ReadRate,
    taken: usize,
}

impl Cursor {
    /// A cursor at `origin`, stepping by `step`.
    fn new(origin: SamplePosition, step: ReadRate) -> Self {
        Self {
            origin,
            step,
            taken: 0,
        }
    }

    /// The position of the input frame `t` past the next.
    fn position(&self, t: usize) -> SamplePosition {
        self.origin + self.step.advance(Samples::new(self.taken + t))
    }

    /// Steps by `step` from here on: the position carries on from where the last
    /// step left it.
    fn retarget(&mut self, step: ReadRate) {
        if step != self.step {
            self.origin = self.position(0);
            self.taken = 0;
            self.step = step;
        }
    }
}

/// A source a vocoder is fed from.
pub(crate) trait Feed<F: SamplerFloat> {
    /// Source frames between two frames the vocoder takes in.
    fn step(&self) -> ReadRate;

    /// Whether the source can be read before a seat.
    fn primes(&self) -> bool;

    /// A new seat: nothing the last read held carries over.
    fn seated(&mut self);

    /// Reads `positions` (`None`: silence) into the first lanes of `raw`,
    /// frame `i` into frame `i`, gained, in the read `generation` (the seat's:
    /// a new one is a jump wherever it lands).
    fn read(&mut self, positions: &[Option<SamplePosition>], generation: u64, raw: &mut Lanes<F>);

    /// A piece in which nothing sounds.
    fn idle(&mut self);
}

/// A memory voice's source.
pub(crate) struct MemoryFeed<'a, F: SamplerFloat> {
    pub(crate) sampler: &'a MemorySource<F>,
    pub(crate) direction: Direction,
    pub(crate) gain: F,
    pub(crate) n: usize,
    pub(crate) gather: &'a mut Gather<F>,
}

impl<F: SamplerFloat> Feed<F> for MemoryFeed<'_, F> {
    fn step(&self) -> ReadRate {
        self.sampler.read_rate()
    }

    fn primes(&self) -> bool {
        true
    }

    fn seated(&mut self) {}

    fn read(&mut self, positions: &[Option<SamplePosition>], _: u64, raw: &mut Lanes<F>) {
        self.sampler.read_placed_lanes(
            positions,
            self.direction,
            raw.lanes_mut(),
            self.n,
            self.gather,
        );
        for lane in raw.lanes_mut().iter_mut().take(self.n) {
            scale(&mut lane[..positions.len()], self.gain);
        }
    }

    fn idle(&mut self) {}
}

/// A disk voice's source.
pub(crate) struct DiskFeed<'a, F: SamplerFloat> {
    pub(crate) reader: &'a mut DiskVoice<F>,
    pub(crate) n: usize,
    pub(crate) gather: &'a mut Gather<F>,
}

impl<F: SamplerFloat> Feed<F> for DiskFeed<'_, F> {
    fn step(&self) -> ReadRate {
        self.reader.input_step()
    }

    fn primes(&self) -> bool {
        false
    }

    fn seated(&mut self) {
        self.reader.flush();
    }

    fn read(&mut self, positions: &[Option<SamplePosition>], generation: u64, raw: &mut Lanes<F>) {
        self.reader
            .read_at(positions, generation, raw, self.n, self.gather);
    }

    fn idle(&mut self) {
        self.reader.idle();
    }
}

/// A slot's vocoder and where it stands.
pub(crate) struct Stretch<'a, F: SamplerFloat> {
    pub(crate) vocoder: &'a mut PhaseVocoder<F>,
    /// The cursor: the source position of the next frame the vocoder takes
    /// in. `None` while unseated, when the vocoder is reset.
    pub(crate) cursor: &'a mut Option<Cursor>,
    /// The seats so far: the generation of the current one.
    pub(crate) seats: &'a mut u64,
    /// Source frames consumed per output frame: [`consumption`] of the stretch.
    pub(crate) consumption: ReadRate,
}

impl<F: SamplerFloat> Stretch<'_, F> {
    /// Unseats: the vocoder is reset, and the next sounding frame seats
    /// again. A voice already unseated is left as it is.
    pub(crate) fn unseat(&mut self) {
        if self.cursor.take().is_some() {
            self.vocoder.reset();
        }
    }
}

/// The scratch a read goes through: the output and input lanes, and the
/// positions the vocoder's input frames are read at.
pub(crate) struct Lent<'a, F: SamplerFloat> {
    pub(crate) out: &'a mut Lanes<F>,
    pub(crate) raw: &'a mut Lanes<F>,
    pub(crate) reads: &'a mut [Option<SamplePosition>],
    /// The channels the lanes carry.
    pub(crate) n: usize,
}

/// Renders output frames `0..gate.len()` of a piece into the first `n` lanes of
/// `lent.out`: where `gate` has a position the vocoder runs, and elsewhere
/// the frames are silence and the voice unseats. Whether any frame sounded.
///
/// Nothing allocates: the lanes and positions are the owner's.
pub(crate) fn render<F: SamplerFloat>(
    mut st: Stretch<'_, F>,
    feed: &mut impl Feed<F>,
    gate: &[Option<SamplePosition>],
    lent: &mut Lent<'_, F>,
) -> bool {
    let frames = gate.len();
    let mut sounded = false;
    let mut i = 0;
    while i < frames {
        let inside = gate[i].is_some();
        let mut j = i + 1;
        while j < frames && gate[j].is_some() == inside {
            j += 1;
        }
        if let Some(origin) = gate[i] {
            sounded = true;
            let mut cursor = match *st.cursor {
                Some(cursor) => cursor,
                None => seat(&mut st, feed, origin, i..j, lent),
            };
            pump(st.vocoder, &mut cursor, feed, *st.seats, lent, i, j - i);
            *st.cursor = Some(cursor);
        } else {
            for lane in lent.out.lanes_mut().iter_mut().take(lent.n) {
                lane[i..j].fill(F::ZERO);
            }
            st.unseat();
        }
        i = j;
    }
    if !sounded {
        feed.idle();
    }
    sounded
}

/// Seats the vocoder at `origin`, the transport's position of output frame
/// `run.start`, and returns the cursor. A source that primes is read from
/// before the seat, for the vocoder's latency and a window of output frames,
/// discarded into the lanes `run` is about to overwrite.
fn seat<F: SamplerFloat>(
    st: &mut Stretch<'_, F>,
    feed: &mut impl Feed<F>,
    origin: SamplePosition,
    run: std::ops::Range<usize>,
    lent: &mut Lent<'_, F>,
) -> Cursor {
    *st.seats += 1;
    feed.seated();
    let step = feed.step();
    if !feed.primes() {
        return Cursor::new(origin, step);
    }
    // The vocoder's output frame `k` is its input timeline's frame `k` at unity, a
    // timeline that starts with `latency` frames of silence. The voice knows its
    // future, so it reads that delay ahead: discarding `prime = latency + window`
    // output frames from source starting a window back puts the first frame after
    // them on the source's own position, `origin`. A stretch consumes `1 / stretch`
    // source frames per output frame.
    let prime = st.vocoder.latency().get() + WINDOW;
    let back = step.then(st.consumption).advance(Samples::new(WINDOW));
    let mut cursor = Cursor::new(origin - back, step);
    let mut left = prime;
    while left > 0 {
        let chunk = left.min(run.len());
        pump(
            st.vocoder,
            &mut cursor,
            feed,
            *st.seats,
            lent,
            run.start,
            chunk,
        );
        left -= chunk;
    }
    cursor
}

/// Runs the vocoder over `frames` output frames into the lanes from frame
/// `at`: for each hop of them it reads the input the vocoder asks for at the
/// cursor and takes it in. An input longer than the scratch is taken in
/// pieces ahead of the output, the vocoder's ring holding what it is given.
fn pump<F: SamplerFloat>(
    vocoder: &mut PhaseVocoder<F>,
    cursor: &mut Cursor,
    feed: &mut impl Feed<F>,
    generation: u64,
    lent: &mut Lent<'_, F>,
    at: usize,
    frames: usize,
) {
    let mut done = 0;
    while done < frames {
        let hop = (frames - done).min(HOP);
        let mut need = vocoder.input_needed(Samples::new(hop)).get();
        while need > lent.reads.len() {
            let ahead = lent.reads.len();
            if take(vocoder, cursor, feed, generation, lent, ahead, (at, 0)) == 0 {
                break;
            }
            need = vocoder.input_needed(Samples::new(hop)).get();
        }
        let count = need.min(lent.reads.len());
        take(
            vocoder,
            cursor,
            feed,
            generation,
            lent,
            count,
            (at + done, hop),
        );
        done += hop;
    }
}

/// Reads `count` frames at the cursor and takes them into the vocoder,
/// writing `output.1` frames of output from frame `output.0`. The input
/// frames the vocoder took, and the cursor moved over them.
fn take<F: SamplerFloat>(
    vocoder: &mut PhaseVocoder<F>,
    cursor: &mut Cursor,
    feed: &mut impl Feed<F>,
    generation: u64,
    lent: &mut Lent<'_, F>,
    count: usize,
    output: (usize, usize),
) -> usize {
    cursor.retarget(feed.step());
    for (t, read) in lent.reads[..count].iter_mut().enumerate() {
        let position = cursor.position(t);
        *read = (position >= WAVE_START).then_some(position);
    }
    feed.read(&lent.reads[..count], generation, lent.raw);
    let input = Planes::from_vecs(&lent.raw.lanes()[..lent.n])
        .expect("BUG: a stretching voice has at least one lane, all as long")
        .slice(Samples::new(0)..Samples::new(count));
    let mut lanes = PlanesMut::from_vecs(&mut lent.out.lanes_mut()[..lent.n])
        .expect("BUG: a stretching voice has at least one lane, all as long");
    let (at, frames) = output;
    let counts = vocoder.process(
        input,
        lanes.slice_mut(Samples::new(at)..Samples::new(at + frames)),
    );
    cursor.taken += counts.consumed.get();
    counts.consumed.get()
}
