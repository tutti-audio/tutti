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
//! and a window more of output frames (`prime`) from source starting a window
//! before the seat and discards them, so the first sounding frame is the source
//! at its own place on the timeline, on time, as a voice that had been playing all
//! along would sound it (the window is the ramp a stretched stream takes to fill).
//! A memory voice then declares no latency. A disk voice's ring reads forward and
//! holds nothing before a seat, so it is not primed: its first [`latency`]
//! frames are silence and it sounds [`latency`] frames late, which a standalone
//! node declares and a pool cannot.
//!
//! # The priming budget
//!
//! `prime` is about 4 100 output frames of vocoder work, several milliseconds, and
//! voices that reseat together (a seek, a loop wrap) would do it in one block each.
//! A seat does at most [`PRIME_SPEED`] frames of it per frame of the block (the
//! rest is carried in `Cursor::owed`), and the voice is silent until it is done.
//! The speed is 2: a block of `L` frames does at most `2 L` frames of priming. A
//! vocoder analysis frame (an FFT pair per channel, a phase solution and the
//! overlap-add) costs about 50 us for a stereo voice, and a hop of output is one, so
//! a block of 256 frames does one frame of priming per voice and 64 voices reseating
//! together take 4.3 ms, under the 5.33 ms a 256-frame block lasts at 48 kHz; one voice
//! takes 63 us in a 64-frame block (the bench `stretch/reseat_*`). Speeds 3, 4 and 8
//! take 6.5, 6.5 and 9.5 ms for the 64 voices. Each frame that goes by adds a frame to
//! what is owed (the voice must discard up to the frame it sounds at), and the
//! speed takes two off, so the voice sounds `ceil(owed / 1)` frames after it was
//! seated: `prime`, 4 097 frames (85 ms at 48 kHz) from its first frame. The speed is
//! per frame, not per block, so when the voice sounds does not depend on how the output
//! is split into blocks. When it does, it is on the timeline: it skips the frames it
//! was too late for. A voice seated at the start of its window, and a seek or a loop
//! wrap, sound that much later; a voice whose window opens ahead (a clip placed on
//! the timeline) starts priming `prime / 2` frames, and a block, before its start,
//! from a transport that rolls, so its first frame lands on its frame (`preroll`). A
//! stopped transport still runs blocks, and a voice whose window covers where it
//! stands seats there and primes at the same rate, so play from a located position
//! sounds at once once the transport has stood for `prime / 2` frames; a locate while
//! stopped seats again.
//!
//! The cursor is an origin and the frames taken since: a position is the origin
//! plus the step times all of them, one product, so the frames a source is read at
//! do not depend on how the output was split into blocks.
//!
//! The sampler relies on the vocoder's invariant that a non-finite input sample
//! does not latch (the output is clean again once the frames holding it have gone
//! out, with no reset), and does not reset it after a block that wrote one.
//!
//! [`latency`]: PhaseVocoder::latency

use tutti_buffers::{ChannelLayout, Planes, PlanesMut};
use tutti_dsp::{
    ColaGeometry, HopSize, PhaseVocoder, PhaseVocoderControls, StftGeometry, WindowLength,
};
use tutti_units::{Beat, ReadRate, SamplePosition, SampleRate, Samples};

use super::disk_voice::DiskVoice;
use super::memory_source::MemorySource;
use super::types::{Direction, Playback};
use crate::SamplerFloat;
use crate::lanes::{Gather, Lanes, scale};

/// The vocoder's analysis window: about 43 ms at 48 kHz.
const WINDOW: Samples = Samples::new(2048);

/// The vocoder's hop: a quarter of the window, the overlap Hann squared needs
/// to sum to a constant.
const HOP: Samples = Samples::new(512);

/// The frames of priming a seat does per frame of the block it is in (see the module
/// docs).
pub(crate) const PRIME_SPEED: usize = 2;

/// The length of the scratch the discarded output of priming goes to: one hop, what a
/// vocoder call writes at most.
pub(crate) const PRIME_SCRATCH: Samples = HOP;

/// The first frame of a wave: before it a read gives silence.
const WAVE_START: SamplePosition = SamplePosition::new(0.0);

/// The sample rate the vocoder's geometry names: its arithmetic counts frames and
/// never reads it.
const NOMINAL_RATE: SampleRate = SampleRate::SR_48K;

/// A phase vocoder over `width` channels on the voice's fixed geometry (a
/// 2048-frame Hann window, hop 512). Allocates: control thread only.
pub(crate) fn vocoder<F: SamplerFloat>(width: ChannelLayout) -> Box<PhaseVocoder<F>> {
    let window = WindowLength::new(WINDOW).expect("BUG: a nonzero window");
    let hop = HopSize::new(HOP).expect("BUG: a nonzero hop");
    let geometry = ColaGeometry::try_from(StftGeometry::new(NOMINAL_RATE, window, hop))
        .expect("BUG: a hop of a quarter window is COLA-valid");
    Box::new(PhaseVocoder::new(width, geometry))
}

/// The output frames a seat primes: the vocoder's latency and a window.
pub(crate) fn prime<F: SamplerFloat>(vocoder: &PhaseVocoder<F>) -> Samples {
    Samples::new(vocoder.latency().get() + WINDOW.get())
}

/// The controls a voice's intent asks of its vocoder.
pub(crate) fn controls(play: &Playback) -> PhaseVocoderControls {
    PhaseVocoderControls {
        stretch: play.stretch,
        pitch: play.pitch,
    }
}

/// Where the vocoder's next input frame is read in the source, and what is left of
/// its priming.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor {
    /// Where the frame `taken` frames ago was read, before the step last changed.
    origin: SamplePosition,
    /// Source frames per input frame taken since `origin`.
    step: ReadRate,
    taken: Samples,
    /// The output frames of priming still to run before the voice sounds.
    owed: Samples,
    /// Whether the seat was made ahead of the voice's window, which the voice has
    /// not yet entered.
    preroll: bool,
    /// The beat a stopped transport stood at when the seat was made there, so a locate
    /// is seen as one.
    at: Option<Beat>,
}

impl Cursor {
    /// A cursor at `origin`, stepping by `step`, owing `owed` frames.
    fn new(origin: SamplePosition, step: ReadRate, owed: Samples) -> Self {
        Self {
            origin,
            step,
            taken: Samples::new(0),
            owed,
            preroll: false,
            at: None,
        }
    }

    /// The position of the input frame `t` past the next.
    fn position(&self, t: usize) -> SamplePosition {
        self.origin + self.step.advance(Samples::new(self.taken.get() + t))
    }

    /// Steps by `step` from here on: the position carries on from where the last
    /// step left it.
    fn retarget(&mut self, step: ReadRate) {
        if step != self.step {
            self.origin = self.position(0);
            self.taken = Samples::new(0);
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
    /// Source frames consumed per output frame: the vocoder's [`read_rate`](PhaseVocoder::read_rate).
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

    /// Seats the vocoder at `origin`, the source's position at the frame the
    /// voice sounds from, owing its priming (see the module docs).
    fn seat(&mut self, feed: &mut impl Feed<F>, origin: SamplePosition) -> Cursor {
        *self.seats += 1;
        feed.seated();
        let step = feed.step();
        if !feed.primes() {
            return Cursor::new(origin, step, Samples::new(0));
        }
        // The vocoder's output frame `k` is its input timeline's frame `k` at
        // unity, a timeline that starts with `latency` frames of silence. The voice
        // knows its future, so it reads that delay ahead: discarding `prime` output
        // frames from source starting a window back puts the first frame after
        // them on the source's own position, `origin`. A stretch consumes
        // `1 / stretch` source frames per output frame.
        let back = step.then(self.consumption).advance(WINDOW);
        Cursor::new(origin - back, step, prime(self.vocoder))
    }
}

/// The input side of a read: the lanes the source is read into and the positions
/// it is read at.
pub(crate) struct Input<'a, F: SamplerFloat> {
    pub(crate) raw: &'a mut Lanes<F>,
    pub(crate) reads: &'a mut [Option<SamplePosition>],
    /// The channels the lanes carry.
    pub(crate) n: usize,
}

/// Discards `frames` output frames of priming into `prime`, the scratch (at least
/// [`PRIME_SCRATCH`] long), a hop at a time.
fn discard<F: SamplerFloat>(
    vocoder: &mut PhaseVocoder<F>,
    cursor: &mut Cursor,
    feed: &mut impl Feed<F>,
    generation: u64,
    input: &mut Input<'_, F>,
    prime: &mut Lanes<F>,
    frames: usize,
) {
    let mut left = frames;
    while left > 0 {
        let hop = left.min(PRIME_SCRATCH.get());
        pump(vocoder, cursor, feed, generation, input, prime, 0, hop);
        left -= hop;
    }
}

/// Spends the priming `cursor` owes over a run of `run` frames, and returns the
/// frames of the run it is still silent for: the cursor owes `owed` frames to sound
/// at the run's first frame, each frame of the run adds one, and the speed takes
/// [`PRIME_SPEED`] off. Priming that finishes `k` frames in has discarded `owed + k`
/// and sounds from there; priming that does not has discarded `PRIME_SPEED * run`
/// and owes the rest, with the run gone by.
fn spend<F: SamplerFloat>(
    vocoder: &mut PhaseVocoder<F>,
    cursor: &mut Cursor,
    feed: &mut impl Feed<F>,
    generation: u64,
    input: &mut Input<'_, F>,
    prime: &mut Lanes<F>,
    run: usize,
) -> usize {
    let owed = cursor.owed.get();
    if owed == 0 {
        return 0;
    }
    let k = owed.div_ceil(PRIME_SPEED - 1);
    if k <= run {
        discard(vocoder, cursor, feed, generation, input, prime, owed + k);
        cursor.owed = Samples::new(0);
        k
    } else {
        discard(
            vocoder,
            cursor,
            feed,
            generation,
            input,
            prime,
            PRIME_SPEED * run,
        );
        cursor.owed = Samples::new(owed - (PRIME_SPEED - 1) * run);
        run
    }
}

/// Starts or goes on priming a voice whose window opens `ahead` frames after the first
/// of a block of `block` frames, seated at `origin`, its position at the window's
/// start; nothing is owed for the frames before the window. The voice may still owe
/// what it can run [`PRIME_SPEED`] frames per frame in the frames that are left, which
/// is what is kept: so the priming is done by the window's start and does not depend on
/// how the output is split into blocks. A block that has the window's start in it
/// finishes the priming (the caller starts priming at least `prime / PRIME_SPEED`
/// frames ahead). The voice keeps its seat in `render` until it enters its window.
///
/// A stopped transport is `standing`: the beat it stands at, with `ahead` 0 and `origin`
/// the source's position at that beat. The voice seats there and primes at the same
/// rate, one block's worth a block, so play from where the playhead stands sounds at
/// once; a locate (another beat) seats again.
#[allow(clippy::too_many_arguments)] // a seat: the vocoder, its source, and where each goes
pub(crate) fn preroll<F: SamplerFloat>(
    st: &mut Stretch<'_, F>,
    feed: &mut impl Feed<F>,
    origin: SamplePosition,
    standing: Option<Beat>,
    ahead: usize,
    block: usize,
    input: &mut Input<'_, F>,
    prime: &mut Lanes<F>,
) {
    if standing.is_some() && st.cursor.is_some_and(|cursor| cursor.at != standing) {
        st.unseat();
    }
    let mut cursor = match *st.cursor {
        Some(cursor) => cursor,
        None => {
            let mut cursor = st.seat(feed, origin);
            cursor.preroll = true;
            cursor.at = standing;
            cursor
        }
    };
    let keep = PRIME_SPEED * (ahead - ahead.min(block));
    let frames = cursor
        .owed
        .get()
        .saturating_sub(keep)
        .min(PRIME_SPEED * block);
    discard(
        st.vocoder,
        &mut cursor,
        feed,
        *st.seats,
        input,
        prime,
        frames,
    );
    cursor.owed = Samples::new(cursor.owed.get() - frames);
    *st.cursor = Some(cursor);
}

/// Renders output frames `0..gate.len()` of a piece into the first `input.n` lanes of
/// `out`: where `gate` has a position the vocoder runs, and elsewhere the frames
/// are silence and the voice unseats (a voice that is priming ahead of its window
/// keeps its seat). While a seat still owes priming the frames are silence too, and
/// the priming goes through `prime`. Whether any frame sounded.
///
/// Nothing allocates: the lanes and positions are the owner's.
pub(crate) fn render<F: SamplerFloat>(
    mut st: Stretch<'_, F>,
    feed: &mut impl Feed<F>,
    gate: &[Option<SamplePosition>],
    out: &mut Lanes<F>,
    prime: &mut Lanes<F>,
    input: &mut Input<'_, F>,
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
                None => st.seat(feed, origin),
            };
            cursor.preroll = false;
            // Silent while it primes: the frames of the run before it sounds.
            let silent = spend(
                st.vocoder,
                &mut cursor,
                feed,
                *st.seats,
                input,
                prime,
                j - i,
            );
            for lane in out.lanes_mut().iter_mut().take(input.n) {
                lane[i..i + silent].fill(F::ZERO);
            }
            if silent < j - i {
                pump(
                    st.vocoder,
                    &mut cursor,
                    feed,
                    *st.seats,
                    input,
                    out,
                    i + silent,
                    j - i - silent,
                );
            }
            *st.cursor = Some(cursor);
        } else {
            for lane in out.lanes_mut().iter_mut().take(input.n) {
                lane[i..j].fill(F::ZERO);
            }
            if !st.cursor.is_some_and(|cursor| cursor.preroll) {
                st.unseat();
            }
        }
        i = j;
    }
    if !sounded {
        feed.idle();
    }
    sounded
}

/// Runs the vocoder over `frames` output frames into `out` from frame `at`: for
/// each hop of them it reads the input the vocoder asks for at the cursor and takes
/// it in. An input longer than the scratch is taken in pieces ahead of the output,
/// the vocoder's ring holding what it is given.
#[allow(clippy::too_many_arguments)] // one run: the vocoder, its source, and where each goes
fn pump<F: SamplerFloat>(
    vocoder: &mut PhaseVocoder<F>,
    cursor: &mut Cursor,
    feed: &mut impl Feed<F>,
    generation: u64,
    input: &mut Input<'_, F>,
    out: &mut Lanes<F>,
    at: usize,
    frames: usize,
) {
    let mut done = 0;
    while done < frames {
        let hop = (frames - done).min(HOP.get());
        let mut need = vocoder.input_needed(Samples::new(hop)).get();
        while need > input.reads.len() {
            let ahead = input.reads.len();
            if take(
                vocoder,
                cursor,
                feed,
                generation,
                input,
                out,
                ahead,
                (at, 0),
            ) == 0
            {
                break;
            }
            need = vocoder.input_needed(Samples::new(hop)).get();
        }
        let count = need.min(input.reads.len());
        take(
            vocoder,
            cursor,
            feed,
            generation,
            input,
            out,
            count,
            (at + done, hop),
        );
        done += hop;
    }
}

/// Reads `count` frames at the cursor and takes them into the vocoder,
/// writing `output.1` frames of output from frame `output.0` of `out`. The input
/// frames the vocoder took, and the cursor moved over them.
#[allow(clippy::too_many_arguments)] // one chunk: the vocoder, its source, and where each goes
fn take<F: SamplerFloat>(
    vocoder: &mut PhaseVocoder<F>,
    cursor: &mut Cursor,
    feed: &mut impl Feed<F>,
    generation: u64,
    input: &mut Input<'_, F>,
    out: &mut Lanes<F>,
    count: usize,
    output: (usize, usize),
) -> usize {
    cursor.retarget(feed.step());
    for (t, read) in input.reads[..count].iter_mut().enumerate() {
        let position = cursor.position(t);
        *read = (position >= WAVE_START).then_some(position);
    }
    feed.read(&input.reads[..count], generation, input.raw);
    let planes = Planes::from_vecs(&input.raw.lanes()[..input.n])
        .expect("BUG: a stretching voice has at least one lane, all as long")
        .slice(Samples::new(0)..Samples::new(count));
    let mut lanes = PlanesMut::from_vecs(&mut out.lanes_mut()[..input.n])
        .expect("BUG: a stretching voice has at least one lane, all as long");
    let (at, frames) = output;
    let counts = vocoder.process(
        planes,
        lanes.slice_mut(Samples::new(at)..Samples::new(at + frames)),
    );
    cursor.taken = Samples::new(cursor.taken.get() + counts.consumed.get());
    counts.consumed.get()
}
