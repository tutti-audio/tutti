//! A streaming phase vocoder: time-stretch and pitch-shift of any width, over the STFT
//! vocabulary.
//!
//! # Shape
//!
//! The vocoder is pulled by its output. [`PhaseVocoder::input_needed`] says how many input
//! frames the next `n` output frames consume under the controls in force, and
//! [`PhaseVocoder::process`] takes input and fills output, using what it needs and keeping up
//! to its ring's room of what it was given beyond that. A source whose ratio varies (the
//! sampler's voice) asks, reads exactly that many frames from wherever its cursor is, and
//! calls `process`. A graph effect hands `process` each block's input and output as they come
//! and builds the vocoder with [`PhaseVocoder::fixed_rate`], which starts the stream with
//! [`latency`](PhaseVocoder::latency) frames of silence ahead: `n` input frames are then always
//! enough for `n` output frames under any pitch, whatever the block size and wherever it falls
//! on the hop. ([`PhaseVocoder::new`] leads the stream with the window alone, which is enough
//! at unity.)
//!
//! # Algorithm
//!
//! - *Analysis* windows frames of the input at a position that advances by
//!   `hop / (stretch * pitch ratio)` per frame and is kept fractional. A frame is read at the
//!   nearest whole sample and the phase advance is measured over the whole-sample step
//!   actually taken, so no sample goes through an interpolator, is dropped or is repeated: the
//!   position's error (under half a sample) is absorbed by the measured advance.
//! - *Phase solution* is identity phase locking on the channel sum (see the private `lock`),
//!   so the channels share one solution.
//! - *Synthesis* inverse-transforms, windows and overlap-adds at the geometry's hop, and
//!   finishes each hop by dividing out the window energy accumulated for it.
//! - *Pitch* is the stretch by the pitch ratio followed by a cubic read of the finished output
//!   at that ratio per output frame ([`cubic_hermite`]; a whole position reads the frame
//!   itself), so a shift is a resample of a stretch, and a change of either applies at the next
//!   frame with the phase accumulators carrying on. A hop of the finished output is read at
//!   the step it was made at, so the input a frame spans and the output it fills agree
//!   however the controls move in between.

mod lock;
#[cfg(test)]
mod tests;

use tutti_buffers::{ChannelLayout, Mono, MonoMut, Planes, PlanesMut};
use tutti_units::{Cents, ReadRate, SamplePosition, Samples, StretchFactor};

use crate::fft::{Complex, FftFloat, RealFft};
use crate::interp::cubic_hermite;
use crate::stft::geometry::{ColaGeometry, StftGeometry};

use lock::PhaseLock;

/// The farthest the pitch is taken from unison, either way: two octaves.
const MAX_PITCH: Cents = Cents::new(2400.0);

/// The controls of a [`PhaseVocoder`], read at the next hop after they are retargeted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaseVocoderControls {
    /// Output duration over input duration, within [`StretchFactor::MIN`] and
    /// [`StretchFactor::MAX`]; a non-finite value is read as unity. A
    /// [`fixed_rate`](PhaseVocoder::fixed_rate) vocoder ignores it: it takes as many frames as
    /// it gives, so its stretch is always unity.
    pub stretch: StretchFactor,
    /// The pitch shift, within two octaves either way, at the duration the stretch gives; a
    /// non-finite value is read as no shift.
    pub pitch: Cents,
}

impl PhaseVocoderControls {
    /// No stretch and no shift.
    pub const UNITY: Self = Self {
        stretch: StretchFactor::UNITY,
        pitch: Cents::new(0.0),
    };
}

/// What one [`process`](PhaseVocoder::process) call moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamCounts {
    /// The input frames taken, the first this many of the input view.
    pub consumed: Samples,
    /// The output frames written, the first this many of the output view; the rest is
    /// silence, written when the input ran out first.
    pub produced: Samples,
}

/// Where the cubic read of the finished output stands: the position of frame `frames` past
/// `base` is `base + step * frames`, one product, so it depends on the hop it is in and not on
/// how the output was split into calls. `base` is folded forward at each hop of the finished
/// output the read leaves.
#[derive(Clone, Copy, Debug)]
struct ReadCursor {
    base: SamplePosition,
    frames: usize,
}

/// Where the stream stands, in the timeline of the input (which starts with the
/// [`latency`](PhaseVocoder::latency) frames of silence fed ahead) and of the output.
#[derive(Clone, Copy, Debug)]
struct Cursor {
    /// The input frames fed so far, the silence included.
    fed: usize,
    /// Where the next frame's window starts in the input, fractional.
    position: SamplePosition,
    /// The whole-sample start of the last frame, once there is one.
    last_start: Option<usize>,
    /// The output frames finished so far: the next frame's first output sample.
    finished: usize,
    /// Where the next output frame reads the finished output, fractional.
    read: ReadCursor,
}

impl Cursor {
    /// The whole-sample start of the next frame: the position rounded, and past the last
    /// frame's start (a hop under one sample steps one sample).
    fn next_start(&self) -> usize {
        let start = libm::round(self.position.get()) as usize;
        self.last_start.map_or(start, |last| start.max(last + 1))
    }

    /// Advances over a frame that started at `start`: the position moves through the input by
    /// `rate` per output frame of the hop.
    fn frame_done(&mut self, start: usize, hop: usize, rate: ReadRate) {
        self.position += rate.advance(Samples::new(hop));
        self.last_start = Some(start);
        self.finished += hop;
    }
}

/// A phase vocoder over `width` channels in the float `F`.
///
/// Built on the control side: every buffer is sized there, and nothing after allocates. One
/// phase solution serves every channel (see the module docs), so a stereo image survives
/// the stretch and identical channels stay identical, bit for bit. Both controls apply at the
/// next hop with no flush: the phase history carries on across a change.
///
/// A non-finite input sample reaches the output for the frames whose windows hold it, and the
/// vocoder is clean again once they have passed: nothing latches, and no reset is needed.
///
/// ```
/// use tutti_buffers::{ChannelLayout, Planes, PlanesMut};
/// use tutti_dsp::{
///     ColaGeometry, HopSize, PhaseVocoder, PhaseVocoderControls, StftGeometry, WindowLength,
/// };
/// use tutti_units::{Cents, SampleRate, Samples, StretchFactor};
///
/// let geometry = StftGeometry::new(
///     SampleRate::SR_48K,
///     WindowLength::new(Samples::new(1024)).unwrap(),
///     HopSize::new(Samples::new(256)).unwrap(),
/// );
/// let mut vocoder =
///     PhaseVocoder::<f32>::new(ChannelLayout::MONO, ColaGeometry::try_from(geometry).unwrap());
/// vocoder.retarget(&PhaseVocoderControls {
///     stretch: StretchFactor::new(1.5),
///     pitch: Cents::new(100.0),
/// });
///
/// // Ask what 512 output frames take, read that much from the source, and process.
/// let needed = vocoder.input_needed(Samples::new(512)).get();
/// let source = vec![0.25f32; needed];
/// let mut out = vec![0.0f32; 512];
/// let counts = vocoder.process(
///     Planes::new(&[&source]).unwrap(),
///     PlanesMut::new(&mut [&mut out]).unwrap(),
/// );
/// assert_eq!((counts.consumed.get(), counts.produced.get()), (needed, 512));
/// ```
pub struct PhaseVocoder<F: FftFloat> {
    geometry: StftGeometry,
    /// The analysis and synthesis window.
    window: Vec<F>,
    plan: RealFft<F>,
    scratch: Vec<Complex<F>>,
    /// One frame in the time domain.
    frame: Vec<F>,
    /// One spectrum per channel.
    spectra: Vec<Vec<Complex<F>>>,
    lock: PhaseLock<F>,
    /// The input, one ring per channel, indexed by timeline sample modulo its length.
    input: Vec<Vec<F>>,
    /// The overlap-add sum, one ring per channel, and the window energy summed beside it.
    sum: Vec<Vec<F>>,
    energy: Vec<F>,
    /// The finished output, one ring per channel.
    finished: Vec<Vec<F>>,
    /// The window energy under which an output sample is silence rather than a quotient.
    energy_floor: F,
    cursor: Cursor,
    /// How far a frame's window moves through the input per output frame:
    /// `1 / (stretch * ratio)`, `ratio` being the pitch ratio.
    advance: ReadRate,
    /// How far the cubic read moves through the finished output per output frame: `ratio`.
    step: ReadRate,
    /// The `step` in force when each hop of the finished output was made, by hop index modulo
    /// its length: a hop is read at the step it was made at, so the input a frame spans and the
    /// output it fills agree whatever the controls do in between.
    steps: Vec<ReadRate>,
    /// The source frames consumed per output frame, `1 / stretch` as retargeted.
    consumption: ReadRate,
    /// The frames of silence the stream is led with beyond the window's: nonzero in the
    /// fixed-rate mode, which also holds the stretch at unity.
    lead: usize,
}

impl<F: FftFloat> std::fmt::Debug for PhaseVocoder<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhaseVocoder")
            .field("geometry", &self.geometry)
            .field("channels", &self.input.len())
            .field("advance", &self.advance)
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl<F: FftFloat> PhaseVocoder<F> {
    /// A vocoder of `width` channels on `geometry`, at unity, pulled by its output (see the
    /// module docs): the caller asks [`input_needed`](Self::input_needed) per block.
    #[must_use]
    pub fn new(width: ChannelLayout, geometry: ColaGeometry) -> Self {
        Self::with_lead(width, geometry, 0)
    }

    /// A vocoder of `width` channels on `geometry` that runs at a fixed rate: every call of
    /// [`process`](Self::process) takes `n` input frames and gives `n` output frames, for any
    /// `n` and any [`pitch`](PhaseVocoderControls::pitch) in range, however it changes between
    /// calls. The stretch is held at unity: the
    /// [`stretch`](PhaseVocoderControls::stretch) control is ignored, silently, because a
    /// stretch would make the frames taken differ from the frames given.
    ///
    /// A frame steps through the input by up to `hop / floor` frames, `floor` being the lowest
    /// ratio of the step (a shift two octaves down, or `hop / window`), and the cubic read
    /// looks three finished frames ahead, which is `3 / floor` input frames. The stream is led
    /// with silence by that much more than the window's, and [`latency`](Self::latency)
    /// includes it, so an input frame the next frame needs is always already given.
    #[must_use]
    pub fn fixed_rate(width: ChannelLayout, geometry: ColaGeometry) -> Self {
        let geometry_ = StftGeometry::from(geometry);
        let (size, hop) = (
            geometry_.window().get() as f64,
            geometry_.hop().get() as f64,
        );
        let floor = Cents::new(-MAX_PITCH.get())
            .to_pitch_ratio()
            .max(hop / size);
        // One frame's step, the cubic read's reach, and a frame of rounding.
        let lead = libm::ceil(hop / floor + 3.0 / floor) as usize + 1;
        Self::with_lead(width, geometry, lead)
    }

    fn with_lead(width: ChannelLayout, geometry: ColaGeometry, lead: usize) -> Self {
        let geometry = StftGeometry::from(geometry);
        let (size, hop) = (geometry.window().get(), geometry.hop().get());
        let width = usize::from(width.count());
        let plan = RealFft::new(size);
        let window = geometry.window_coefficients::<F>();
        let zero = Complex::new(F::ZERO, F::ZERO);
        let bins = geometry.bins_per_frame().get();
        // The energy of the window frames that overlap one output sample, at a frame's first
        // sample: what a fully covered sample accumulates.
        let full: f64 = (0..size / hop)
            .map(|m| {
                let w: f64 = window[m * hop].into();
                w * w
            })
            .sum();
        let finished_len = (6 * hop + 8).next_power_of_two();
        let mut vocoder = Self {
            geometry,
            scratch: plan.new_scratch(),
            plan,
            frame: vec![F::ZERO; size],
            spectra: vec![vec![zero; bins]; width],
            lock: PhaseLock::new(bins),
            input: vec![vec![F::ZERO; (2 * size + 2 * hop + 8 + lead).next_power_of_two()]; width],
            sum: vec![vec![F::ZERO; size.next_power_of_two()]; width],
            energy: vec![F::ZERO; size.next_power_of_two()],
            finished: vec![vec![F::ZERO; finished_len]; width],
            energy_floor: F::from_design(1e-3 * full),
            window,
            cursor: Cursor {
                fed: 0,
                position: SamplePosition::new(0.0),
                last_start: None,
                finished: 0,
                read: ReadCursor {
                    base: SamplePosition::new(0.0),
                    frames: 0,
                },
            },
            advance: ReadRate::UNITY,
            step: ReadRate::UNITY,
            consumption: ReadRate::UNITY,
            steps: vec![ReadRate::UNITY; (finished_len / hop).max(1)],
            lead,
        };
        vocoder.reset();
        vocoder
    }

    /// The frames from an input impulse to its output at unity stretch and no shift: the
    /// window, which a frame waits to fill, and the one frame of the cubic read's lookahead
    /// beyond it.
    ///
    /// The delay is the silence `new` and `reset` feed ahead of the stream, so that every block
    /// of `n` input frames has `n` output frames finished at unity, whatever the block size and
    /// phase of the hop. A stretch or a shift moves where the window centre lands in the
    /// output, not this figure.
    #[must_use]
    pub fn latency(&self) -> Samples {
        Samples::new(self.geometry.window().get() + 1 + self.lead)
    }

    /// Reads `controls` from the next hop on; the phase history carries on. No allocation.
    ///
    /// A frame cannot step through the input by more than its own length, or the phase
    /// advance of a bin is no longer measurable: `stretch * ratio` is held at `hop / window`
    /// or above, by raising the pitch ratio of a shift down that would take it below.
    pub fn retarget(&mut self, controls: &PhaseVocoderControls) {
        let stretch = Some(controls.stretch.get())
            .filter(|s| s.is_finite())
            .map_or(1.0, |s| {
                s.clamp(StretchFactor::MIN.get(), StretchFactor::MAX.get())
            });
        // A fixed-rate vocoder takes as many frames as it gives: no stretch.
        let stretch = if self.lead == 0 { stretch } else { 1.0 };
        let pitch = Some(controls.pitch.get())
            .filter(|c| c.is_finite())
            .map_or(0.0, |c| c.clamp(-MAX_PITCH.get(), MAX_PITCH.get()));
        let shortest = self.geometry.hop().get() as f64 / self.geometry.window().get() as f64;
        let ratio = Cents::new(pitch).to_pitch_ratio().max(shortest / stretch);
        self.step = ReadRate::new(ratio);
        self.consumption = ReadRate::new(1.0 / stretch);
        self.advance = ReadRate::new(1.0 / (stretch * ratio));
    }

    /// The source frames the stream consumes per output frame under the controls in force,
    /// over a long run: the reciprocal of the stretch as the vocoder reads it (clamped to
    /// range, a non-finite value as unity, and always unity for a
    /// [`fixed_rate`](Self::fixed_rate) vocoder). The pitch does not move it: a shift
    /// resamples the stretch and takes no more input. Frames already held, and a frame in
    /// flight, make a short stretch of output differ from this by up to a window.
    #[must_use]
    pub fn read_rate(&self) -> ReadRate {
        self.consumption
    }

    /// Back to silence at the controls in force: the stream starts again, with
    /// [`latency`](Self::latency) frames of silence ahead of it.
    pub fn reset(&mut self) {
        for ring in self
            .input
            .iter_mut()
            .chain(&mut self.sum)
            .chain(&mut self.finished)
        {
            ring.fill(F::ZERO);
        }
        self.energy.fill(F::ZERO);
        self.lock.reset();
        self.cursor = Cursor {
            fed: self.latency().get(),
            position: SamplePosition::new(0.0),
            last_start: None,
            finished: 0,
            read: ReadCursor {
                base: SamplePosition::new(0.0),
                frames: 0,
            },
        };
    }

    /// Back to silence like [`reset`](Self::reset), with the stream starting at the first
    /// input frame given: no silence leads it. A caller that holds the signal's own history
    /// seats a stream on it, feeding the history from `window` frames before the place it
    /// wants to hear.
    ///
    /// Frame `k` of the output is then the input's frame `k` at unity (the window's
    /// lookahead is the caller's to read ahead of), and the output over the first `window`
    /// frames is a ramp: the analysis frames that overlap it began before the history did.
    /// From frame `window` on it is steady, as a stream that had run all along. The frame
    /// whose output lies wholly inside the ramp needs no synthesis
    /// ([`discard`](Self::discard)).
    pub fn seat(&mut self) {
        self.reset();
        self.cursor.fed = 0;
    }

    /// The input frames the next `output` frames consume under the controls in force, beyond
    /// the input already held. No allocation.
    #[must_use]
    pub fn input_needed(&self, output: Samples) -> Samples {
        let hop = self.geometry.hop().get();
        let size = self.geometry.window().get();
        let mut cursor = self.cursor;
        let mut required = 0;
        let mut left = output.get();
        while left > 0 {
            let n = left.min(hop);
            let need = self.finished_for(cursor.read, n);
            while cursor.finished < need {
                let start = cursor.next_start();
                required = start + size;
                cursor.frame_done(start, hop, self.advance);
            }
            cursor.read = self.walk(cursor.read, n, |_, _, _, _, _| {});
            left -= n;
        }
        Samples::new(required.saturating_sub(self.cursor.fed))
    }

    /// Fills `output` from `input`, as far as the input reaches.
    ///
    /// Frames are taken from `input` as the output needs them; what `output` does not need is
    /// kept up to the ring's room (about two windows) and counted in
    /// [`consumed`](StreamCounts::consumed), so the caller drops that many frames from the
    /// front of the input and no more. When the input is short of what `output` needs, the
    /// output is written as far as it reaches, the rest is silence, and
    /// [`produced`](StreamCounts::produced) says how far. The channels of `input` beyond the
    /// vocoder's width are ignored and those short of it read silence; the channels of
    /// `output` beyond it are silent. No allocation.
    pub fn process(
        &mut self,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) -> StreamCounts {
        let wanted = output.frames().get();
        let counts = self.stream(&input, wanted, Some(&mut output));
        for c in 0..output.channels() {
            output.plane_mut(c).samples_mut()[counts.produced.get()..].fill(F::ZERO);
        }
        counts
    }

    /// Takes `input` as [`process`](Self::process) does for `output` frames of output, and
    /// writes none: each frame it runs is analysed and its phases solved, and nothing is
    /// synthesised, overlap-added or read out.
    ///
    /// After `discard`, the cursors, the phase history and the frames taken are bit for bit
    /// what `process` over the same input and the same `output` leaves, wherever in a stream
    /// it is called. What differs is the overlap-add: the frames run here added nothing to
    /// it, and the hops they would have finished are cleared (so nothing from earlier
    /// frames wraps into a later hop), so the output of the next
    /// [`window`](StftGeometry::window) frames is short of the frames that would have
    /// overlapped it, and from there on it is `process`'s. A caller that seats a stream on history ([`seat`](Self::seat)) discards
    /// the frames whose output lies wholly inside the ramp, and the stream's phases are as
    /// if they had been synthesised. The counts are `process`'s: `produced` is the output
    /// frames discarded. No allocation.
    pub fn discard(&mut self, input: Planes<'_, F>, output: Samples) -> StreamCounts {
        self.stream(&input, output.get(), None)
    }

    /// The shared pull of [`process`](Self::process) and [`discard`](Self::discard): a frame
    /// is synthesised and read out into `output` when there is one.
    fn stream(
        &mut self,
        input: &Planes<'_, F>,
        wanted: usize,
        mut output: Option<&mut PlanesMut<'_, '_, F>>,
    ) -> StreamCounts {
        let hop = self.geometry.hop().get();
        let size = self.geometry.window().get();
        let supplied = input.frames().get();
        let mut consumed = 0;
        let mut produced = 0;
        'chunks: while produced < wanted {
            let n = (wanted - produced).min(hop);
            let need = self.finished_for(self.cursor.read, n);
            while self.cursor.finished < need {
                let start = self.cursor.next_start();
                let required = start + size;
                if self.cursor.fed < required {
                    let take = (required - self.cursor.fed).min(supplied - consumed);
                    self.feed(input, consumed, take);
                    consumed += take;
                    if self.cursor.fed < required {
                        break 'chunks;
                    }
                }
                self.run_frame(start, output.is_some());
            }
            if let Some(output) = output.as_deref_mut() {
                self.read_output(output, produced, n);
            } else {
                self.cursor.read = self.walk(self.cursor.read, n, |_, _, _, _, _| {});
            }
            produced += n;
        }
        let room = self.input[0].len() - self.cursor.fed.saturating_sub(self.cursor.next_start());
        let keep = room.min(supplied - consumed);
        self.feed(input, consumed, keep);
        StreamCounts {
            consumed: Samples::new(consumed + keep),
            produced: Samples::new(produced),
        }
    }

    /// Appends `count` frames of `input`, from `from`, to the input rings.
    fn feed(&mut self, input: &Planes<'_, F>, from: usize, count: usize) {
        for (c, ring) in self.input.iter_mut().enumerate() {
            let mask = ring.len() - 1;
            let plane = input.get_plane(c).map(|plane: Mono<'_, F>| plane.samples());
            for i in 0..count {
                ring[(self.cursor.fed + i) & mask] = plane.map_or(F::ZERO, |p| p[from + i]);
            }
        }
        self.cursor.fed += count;
    }

    /// Analyses the frame at `start` and solves its phases; with `synthesise`, rotates its
    /// spectrum and overlap-adds it, finishing one hop of output.
    fn run_frame(&mut self, start: usize, synthesise: bool) {
        let size = self.geometry.window().get();
        let hop = self.geometry.hop().get();
        for (ring, spectrum) in self.input.iter().zip(&mut self.spectra) {
            let mask = ring.len() - 1;
            for (i, (x, &w)) in self.frame.iter_mut().zip(&self.window).enumerate() {
                *x = ring[(start + i) & mask] * w;
            }
            self.plan
                .forward(Mono::new(&self.frame), spectrum, &mut self.scratch);
        }
        let hops = self.cursor.last_start.map(|last| (start - last, hop));
        let rotation = self.lock.solve(&self.spectra, hops, size);
        if synthesise {
            let norm = F::from_design(1.0 / size as f64);
            let mask = self.energy.len() - 1;
            for (c, spectrum) in self.spectra.iter_mut().enumerate() {
                for (bin, &rotate) in spectrum.iter_mut().zip(rotation) {
                    *bin = *bin * rotate;
                }
                self.plan
                    .inverse(spectrum, MonoMut::new(&mut self.frame), &mut self.scratch);
                for (i, (&x, &w)) in self.frame.iter().zip(&self.window).enumerate() {
                    self.sum[c][(self.cursor.finished + i) & mask] += x * norm * w;
                }
            }
            for (i, &w) in self.window.iter().enumerate() {
                self.energy[(self.cursor.finished + i) & mask] += w * w;
            }
        }
        let slot = self.cursor.finished / hop % self.steps.len();
        self.steps[slot] = self.step;
        if synthesise {
            self.finish_hop();
        } else {
            self.clear_hop();
        }
        self.cursor.frame_done(start, hop, self.advance);
    }

    /// Finishes the hop of output at the cursor: each sum divided by the window energy
    /// beside it, written to the finished rings, and the slots cleared.
    fn finish_hop(&mut self) {
        let hop = self.geometry.hop().get();
        let mask = self.energy.len() - 1;
        let out_mask = self.finished[0].len() - 1;
        for t in self.cursor.finished..self.cursor.finished + hop {
            let energy = self.energy[t & mask];
            for (sum, finished) in self.sum.iter_mut().zip(&mut self.finished) {
                finished[t & out_mask] = if energy > self.energy_floor {
                    sum[t & mask] / energy
                } else {
                    F::ZERO
                };
            }
        }
        self.clear_hop();
    }

    /// Clears the overlap-add slots of the hop at the cursor, which the ring reuses for the
    /// hop a window on: whatever earlier frames added there is dropped.
    fn clear_hop(&mut self) {
        let hop = self.geometry.hop().get();
        let mask = self.energy.len() - 1;
        for t in self.cursor.finished..self.cursor.finished + hop {
            for sum in &mut self.sum {
                sum[t & mask] = F::ZERO;
            }
            self.energy[t & mask] = F::ZERO;
        }
    }

    /// The step a read at hop `index` of the finished output moves by: the one it was made at,
    /// or the one in force for a hop not yet made.
    fn step_of(&self, index: usize) -> ReadRate {
        if index < self.cursor.finished / self.geometry.hop().get() {
            self.steps[index % self.steps.len()]
        } else {
            self.step
        }
    }

    /// Moves a read from `from` through `count` output frames, calling `each(base, step,
    /// first, run, offset)` for every run of frames in one hop of the finished output: frame
    /// `offset + i` of the walk reads at `base + step * (first + i)`. Returns where the read
    /// ends. The hop's length in frames is fixed by its `base` and step, so the runs a walk
    /// breaks into, and every position, are the same whatever `count` it is cut into.
    fn walk(
        &self,
        from: ReadCursor,
        count: usize,
        mut each: impl FnMut(SamplePosition, ReadRate, usize, usize, usize),
    ) -> ReadCursor {
        let hop = self.geometry.hop().get();
        let mut state = from;
        let mut done = 0;
        while done < count {
            let index = libm::floor(state.base.get()) as usize / hop;
            let step = self.step_of(index);
            let edge = ((index + 1) * hop) as f64 - state.base.get();
            let length = (libm::ceil(edge / step.get()) as usize).max(1);
            if state.frames >= length {
                state = ReadCursor {
                    base: state.base + step.advance(Samples::new(length)),
                    frames: 0,
                };
                continue;
            }
            let run = (length - state.frames).min(count - done);
            each(state.base, step, state.frames, run, done);
            state.frames += run;
            done += run;
        }
        state
    }

    /// The finished frames a read of `n` output frames from `from` needs: through the last
    /// read position's cubic neighbours.
    fn finished_for(&self, from: ReadCursor, n: usize) -> usize {
        let mut last = from.base;
        self.walk(from, n, |base, step, first, run, _| {
            last = base + step.advance(Samples::new(first + run - 1));
        });
        libm::floor(last.get()) as usize + 3
    }

    /// Writes `count` output frames from `at`, reading the finished output at the step each hop
    /// was made at: a cubic read between its frames, and the frame itself on a whole position.
    fn read_output(&mut self, output: &mut PlanesMut<'_, '_, F>, at: usize, count: usize) {
        let mask = self.finished[0].len() - 1;
        let end = self.walk(self.cursor.read, count, |base, step, first, run, offset| {
            for (c, ring) in self.finished.iter().enumerate().take(output.channels()) {
                let mut plane = output.plane_mut(c);
                let out = &mut plane.samples_mut()[at + offset..at + offset + run];
                for (i, y) in out.iter_mut().enumerate() {
                    let position = base + step.advance(Samples::new(first + i));
                    let whole = libm::floor(position.get()) as usize;
                    let t = position.fract();
                    let tap =
                        |offset: usize| ring[whole.wrapping_add(offset).wrapping_sub(1) & mask];
                    *y = if t == 0.0 {
                        tap(1)
                    } else {
                        cubic_hermite(tap(0), tap(1), tap(2), tap(3), F::from_design(t))
                    };
                }
            }
        });
        self.cursor.read = end;
    }
}
