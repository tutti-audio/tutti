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
//! calls `process`. A graph effect at unity hands `process` each block's input and output as
//! they come: [`PhaseVocoder::new`] starts the stream with [`latency`](PhaseVocoder::latency)
//! frames of silence ahead, which is what makes `n` input frames always enough for `n` output
//! frames at unity, whatever the block size and wherever it falls on the hop.
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
//!   frame with the phase accumulators carrying on.

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
    /// [`StretchFactor::MAX`]; a non-finite value is read as unity.
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
    /// Where the cubic read stood when the pitch ratio last changed (the start,
    /// before a change), fractional.
    read: SamplePosition,
    /// The output frames read since `read`: a read position is `read` plus the step
    /// times these, one product, so it does not depend on how the output was split
    /// into calls.
    read_frames: usize,
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

    /// Where the cubic read of output frame `k` past the next one stands.
    fn read_at(&self, k: usize, step: ReadRate) -> SamplePosition {
        self.read + step.advance(Samples::new(self.read_frames + k))
    }

    /// The finished frames a read of `n` output frames needs: through the last read
    /// position's cubic neighbours.
    fn finished_for(&self, n: usize, step: ReadRate) -> usize {
        libm::floor(self.read_at(n - 1, step).get()) as usize + 3
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
    /// A vocoder of `width` channels on `geometry`, at unity.
    #[must_use]
    pub fn new(width: ChannelLayout, geometry: ColaGeometry) -> Self {
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
        let mut vocoder = Self {
            geometry,
            scratch: plan.new_scratch(),
            plan,
            frame: vec![F::ZERO; size],
            spectra: vec![vec![zero; bins]; width],
            lock: PhaseLock::new(bins),
            input: vec![vec![F::ZERO; (2 * size + 2 * hop + 8).next_power_of_two()]; width],
            sum: vec![vec![F::ZERO; size.next_power_of_two()]; width],
            energy: vec![F::ZERO; size.next_power_of_two()],
            finished: vec![vec![F::ZERO; (6 * hop + 8).next_power_of_two()]; width],
            energy_floor: F::from_design(1e-3 * full),
            window,
            cursor: Cursor {
                fed: 0,
                position: SamplePosition::new(0.0),
                last_start: None,
                finished: 0,
                read: SamplePosition::new(0.0),
                read_frames: 0,
            },
            advance: ReadRate::UNITY,
            step: ReadRate::UNITY,
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
        Samples::new(self.geometry.window().get() + 1)
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
        let pitch = Some(controls.pitch.get())
            .filter(|c| c.is_finite())
            .map_or(0.0, |c| c.clamp(-MAX_PITCH.get(), MAX_PITCH.get()));
        let shortest = self.geometry.hop().get() as f64 / self.geometry.window().get() as f64;
        let ratio = Cents::new(pitch).to_pitch_ratio().max(shortest / stretch);
        let step = ReadRate::new(ratio);
        if step != self.step {
            self.cursor.read = self.cursor.read_at(0, self.step);
            self.cursor.read_frames = 0;
        }
        self.step = step;
        self.advance = ReadRate::new(1.0 / (stretch * ratio));
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
            read: SamplePosition::new(0.0),
            read_frames: 0,
        };
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
            let need = cursor.finished_for(n, self.step);
            while cursor.finished < need {
                let start = cursor.next_start();
                required = start + size;
                cursor.frame_done(start, hop, self.advance);
            }
            cursor.read_frames += n;
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
        let hop = self.geometry.hop().get();
        let size = self.geometry.window().get();
        let wanted = output.frames().get();
        let supplied = input.frames().get();
        let mut consumed = 0;
        let mut produced = 0;
        'chunks: while produced < wanted {
            let n = (wanted - produced).min(hop);
            let need = self.cursor.finished_for(n, self.step);
            while self.cursor.finished < need {
                let start = self.cursor.next_start();
                let required = start + size;
                if self.cursor.fed < required {
                    let take = (required - self.cursor.fed).min(supplied - consumed);
                    self.feed(&input, consumed, take);
                    consumed += take;
                    if self.cursor.fed < required {
                        break 'chunks;
                    }
                }
                self.run_frame(start);
            }
            self.read_output(&mut output, produced, n);
            produced += n;
        }
        for c in 0..output.channels() {
            output.plane_mut(c).samples_mut()[produced..].fill(F::ZERO);
        }
        let room = self.input[0].len() - self.cursor.fed.saturating_sub(self.cursor.next_start());
        let keep = room.min(supplied - consumed);
        self.feed(&input, consumed, keep);
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

    /// Analyses the frame at `start`, rotates its phases, and overlap-adds it, finishing one
    /// hop of output.
    fn run_frame(&mut self, start: usize) {
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
        self.finish_hop();
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
                sum[t & mask] = F::ZERO;
            }
            self.energy[t & mask] = F::ZERO;
        }
    }

    /// Writes `count` output frames from `at`, reading the finished output at the step: a
    /// cubic read between its frames, and the frame itself on a whole position.
    fn read_output(&mut self, output: &mut PlanesMut<'_, '_, F>, at: usize, count: usize) {
        let mask = self.finished[0].len() - 1;
        for (c, ring) in self.finished.iter().enumerate().take(output.channels()) {
            let mut plane = output.plane_mut(c);
            let out = &mut plane.samples_mut()[at..at + count];
            for (i, y) in out.iter_mut().enumerate() {
                let position = self.cursor.read_at(i, self.step);
                let whole = libm::floor(position.get()) as usize;
                let t = position.fract();
                let tap = |offset: usize| ring[whole.wrapping_add(offset).wrapping_sub(1) & mask];
                *y = if t == 0.0 {
                    tap(1)
                } else {
                    cubic_hermite(tap(0), tap(1), tap(2), tap(3), F::from_design(t))
                };
            }
        }
        self.cursor.read_frames += count;
    }
}
