//! [`SpringReverb`]: a spring's dispersive echoes, after Valimaki, Parker and Abel,
//! "Parametric spring reverberation effect" (2010).
//!
//! A spring carries a pulse as a chirp: its high frequencies travel faster than its low ones.
//! The chirp is a cascade of `M` first-order all-passes whose unit delay is stretched to `K`
//! frames, `A(z) = (z^-K - g) / (1 - g z^-K)`, each of group delay
//! `K (1 - g^2) / (1 + g^2 - 2 g cos(w K))` frames at the angular frequency `w` (radians per
//! frame): for `g > 0` the delay is greatest at DC, `K (1 + g) / (1 - g)`, and least at
//! `w K = pi`, `K (1 - g) / (1 + g)`, so the lows arrive later. Two such cascades of the
//! same `M` and `K` run side by side, the low one at the coefficient `LOW_COEFFICIENT`
//! scaled by the dispersion and the high one at `HIGH_COEFFICIENT` scaled by it, and their
//! mean is the spring's forward path `P = (A_low + A_high) / 2`, of magnitude at most 1.
//!
//! The spring's echo is that path in a loop with a delay of the spring's length and a
//! one-pole low-pass (the tone): `L(z) = g_loop T(z) z^-N P(z)`. `|P| <= 1` and `|T| <= 1`,
//! so `|L| <= g_loop < 1` at every frequency and the loop is stable whatever the dispersion
//! and tone. The loop gain `g_loop` is a [`Feedback`], set so the slowest trip
//! (the delay `N`, the DC group delay of the low cascade and the tone's) of `T_trip` frames
//! falls 60 dB in the decay time `t`: `g_loop = 10^(-3 T_trip / (t fs))`. Every frequency's
//! trip is at most that long, so the response falls 60 dB in at most `t` once the loop gain
//! is not held at the [`Feedback`] ceiling (0.99).
//!
//! **Glide.** The render works in chunks of [`CHUNK`](SpringReverb::CHUNK) frames from the last
//! reset. A new design moves the two cascades' coefficients, the loop gain and the low-pass pole
//! along a [`Ramp`] each, one step per chunk over 20 ms (30 chunks at 48 kHz); the chunk's
//! constants are read at its start. Each ramp runs between two values of its own proven range
//! (a coefficient in `0..=0.6`, a gain in `Feedback`'s, a pole in `0..1`), so every
//! intermediate point is a valid design and `|L| < 1` holds throughout the glide.

use crate::BlockMismatch;
use crate::block::expect_widths;
use core::num::NonZeroUsize;
use tutti_buffers::{ChannelLayout, Float, InputOutput};
use tutti_units::{Feedback, Hz, Mix, SampleRate, Samples, Seconds, Tail};

use crate::delay::trips;
use crate::onepole::cutoff_pole;
use crate::reverb::{CHUNK, ramp_chunks};
use crate::{AllPassFilter, DelayLine, Ramp, TAIL_FLOOR};

/// The stretched unit delay `K` of every all-pass, in frames.
const STRETCH: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// The all-pass coefficient of the low cascade at full dispersion.
pub(crate) const LOW_COEFFICIENT: f64 = 0.6;

/// The all-pass coefficient of the high cascade at full dispersion.
pub(crate) const HIGH_COEFFICIENT: f64 = 0.3;

tutti_units::ranged! {
    /// How many all-passes each cascade of a [`SpringReverb`] has, `1..=512`: the more, the
    /// longer and the more audible the chirp.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    SpringStages(u16), SpringStagesError, "a stage count within 1..=512",
    |raw| raw >= 1 && raw <= 512
}

impl SpringStages {
    /// The count as an index bound.
    #[inline]
    fn len(self) -> usize {
        usize::from(self.get())
    }
}

tutti_units::ranged! {
    /// The length of a [`SpringReverb`]'s loop delay, `5 ms..=1 s`: the time between a spring's
    /// echoes, before the chirp's own delay.
    #[derive(PartialOrd)]
    SpringLength(Seconds), SpringLengthError, "a spring length within 5 ms..=1 s",
    |raw| raw.get().is_finite() && raw.get() >= 0.005 && raw.get() <= 1.0
}

/// What a [`SpringReverb`] is sized by: fixed at construction, since it sets the lines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringLayout {
    /// The stages of each cascade.
    pub stages: SpringStages,
    /// The loop delay.
    pub length: SpringLength,
}

impl SpringLayout {
    /// Forty stages and a loop of 40 ms.
    pub const DEFAULT: Self = Self {
        stages: SpringStages::of(40),
        length: SpringLength::of(Seconds::new(0.04)),
    };
}

impl Default for SpringLayout {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The controls of a [`SpringReverb`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringControls {
    /// The RT60 of the loop's slowest trip, read within [`MIN_DECAY`](Self::MIN_DECAY) and
    /// [`MAX_DECAY`](Self::MAX_DECAY); a loop that cannot hold it (its gain is held at the
    /// [`Feedback`] ceiling) rings at that ceiling. A value that is not finite reads as the
    /// default's.
    pub decay: Seconds,
    /// The cutoff of the loop's one-pole low-pass, read at least [`MIN_TONE`](Self::MIN_TONE);
    /// far above the Nyquist the low-pass is an identity. A value that is not finite reads as
    /// the default's.
    pub tone: Hz,
    /// How far the chirp is dispersed: 0 is a plain delay and 1 the coefficients of
    /// 0.6 for the low cascade and 0.3 for the high one.
    pub dispersion: Mix,
}

impl SpringControls {
    /// A decay of 2 s, a 4 kHz tone and full dispersion.
    pub const DEFAULT: Self = Self {
        decay: Seconds::new(2.0),
        tone: Hz::new(4_000.0),
        dispersion: Mix::WET,
    };
    /// The shortest decay: 100 ms.
    pub const MIN_DECAY: Seconds = Seconds::new(0.1);
    /// The longest decay: 60 s.
    pub const MAX_DECAY: Seconds = Seconds::new(60.0);
    /// The lowest tone cutoff: 20 Hz.
    pub const MIN_TONE: Hz = Hz::new(20.0);

    /// The controls as the design reads them: a figure that is not finite is the default's,
    /// the rest held within its range.
    fn read(self) -> (Seconds, Hz, Mix) {
        let d = Self::DEFAULT;
        let decay = if self.decay.get().is_finite() {
            self.decay
                .get()
                .clamp(Self::MIN_DECAY.get(), Self::MAX_DECAY.get())
        } else {
            d.decay.get()
        };
        let tone = if self.tone.get().is_finite() {
            self.tone.get().max(Self::MIN_TONE.get())
        } else {
            d.tone.get()
        };
        let dispersion = Mix::new_clamped(self.dispersion.get());
        (Seconds::new(decay), Hz::new(tone), dispersion)
    }
}

impl Default for SpringControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What the controls come to at a rate and a layout: the all-pass coefficients, the loop's
/// gain and pole and the slowest trip.
#[derive(Clone, Copy, Debug)]
struct Design {
    low: Feedback,
    high: Feedback,
    loop_gain: Feedback,
    pole: f64,
    /// The slowest trip round the loop, in frames.
    trip: f64,
}

impl Design {
    fn of(layout: SpringLayout, rate: SampleRate, controls: &SpringControls) -> Self {
        let (decay, tone, dispersion) = controls.read();
        let low = Feedback::new_clamped(LOW_COEFFICIENT * dispersion.get());
        let high = Feedback::new_clamped(HIGH_COEFFICIENT * dispersion.get());
        let pole = cutoff_pole(tone, rate);
        let chain =
            (layout.stages.len() * STRETCH.get()) as f64 * (1.0 + low.get()) / (1.0 - low.get());
        let trip = loop_frames(layout, rate) as f64 + chain + pole / (1.0 - pole);
        let loop_gain =
            Feedback::new_clamped(libm::pow(10.0, -3.0 * trip / (decay.get() * rate.get())));
        Self {
            low,
            high,
            loop_gain,
            pole,
            trip,
        }
    }

    /// The frames until the response is under [`TAIL_FLOOR`], erring long: the echoes of the
    /// slowest trip (`1 + trips` of them) and the trip the chain settles in.
    fn tail(&self) -> Tail {
        let echoes = 1.0 + trips(self.loop_gain.get(), TAIL_FLOOR);
        Tail::Finite(Samples::new(((echoes + 1.0) * self.trip).ceil() as usize))
    }
}

/// The loop delay in frames, at least one.
fn loop_frames(layout: SpringLayout, rate: SampleRate) -> usize {
    layout.length.get().to_samples_ceil(rate).get().max(1)
}

/// `stages` all-passes of the stretched delay, each of the coefficient `gain`.
fn cascade<F: Float>(stages: SpringStages, gain: Feedback) -> Vec<AllPassFilter<F>> {
    (0..stages.len())
        .map(|_| AllPassFilter::new(STRETCH, gain))
        .collect()
}

/// One channel's lines.
#[derive(Clone, Debug)]
struct Spring<F: Float> {
    low: Vec<AllPassFilter<F>>,
    high: Vec<AllPassFilter<F>>,
    line: DelayLine<F>,
    /// The low-pass's state.
    tone: F,
}

/// What glides: the two cascades' coefficients, the loop gain and the low-pass pole.
#[derive(Clone, Copy, Debug)]
struct Glide {
    low: Ramp<f64>,
    high: Ramp<f64>,
    gain: Ramp<f64>,
    pole: Ramp<f64>,
}

impl Glide {
    fn at_rest(design: &Design, chunks: u32) -> Self {
        let rest = |v: f64| Ramp::new(v).steps(chunks);
        Self {
            low: rest(design.low.get()),
            high: rest(design.high.get()),
            gain: rest(design.loop_gain.get()),
            pole: rest(design.pole),
        }
    }

    fn each(&mut self) -> [&mut Ramp<f64>; 4] {
        [
            &mut self.low,
            &mut self.high,
            &mut self.gain,
            &mut self.pole,
        ]
    }

    /// Aims every ramp at `design`; a target already set leaves its ramp as it is.
    fn aim(&mut self, design: &Design) {
        let to = [
            design.low.get(),
            design.high.get(),
            design.loop_gain.get(),
            design.pole,
        ];
        for (ramp, to) in self.each().into_iter().zip(to) {
            if ramp.target() != to {
                ramp.retarget(to);
            }
        }
    }
}

/// A spring reverb of any width: every channel is its own spring, `y = (A_low + A_high) / 2`
/// of the input and the loop's return, the loop a delay and a one-pole low-pass with a gain
/// below 1.
///
/// Built on the control side for one rate and layout, where it allocates its lines; nothing
/// after that does. Wet only. A new design glides in over 20 ms of [`CHUNK`](Self::CHUNK)-frame
/// chunks (see the module docs): every point of the glide is a valid design, so the loop
/// stays stable throughout.
#[derive(Clone, Debug)]
pub struct SpringReverb<F: Float> {
    layout: SpringLayout,
    rate: SampleRate,
    design: Design,
    glide: Glide,
    /// The gain, the pole and its complement of the chunk being run.
    gain: F,
    pole: F,
    leave: F,
    /// Frames of the current chunk already run: 0 at a chunk's start.
    into: usize,
    springs: Vec<Spring<F>>,
}

impl<F: Float> SpringReverb<F> {
    /// The frames between the glide's steps: the chunk.
    pub const CHUNK: usize = CHUNK;

    /// A silent spring of `width` channels (an empty layout has none) at `rate`, at
    /// [`SpringControls::DEFAULT`].
    #[must_use]
    pub fn new(width: ChannelLayout, rate: SampleRate, layout: SpringLayout) -> Self {
        let design = Design::of(layout, rate, &SpringControls::DEFAULT);
        let delay = loop_frames(layout, rate);
        let spring = Spring {
            low: cascade(layout.stages, design.low),
            high: cascade(layout.stages, design.high),
            line: DelayLine::new(Samples::new(delay)),
            tone: F::ZERO,
        };
        let mut spring_reverb = Self {
            layout,
            rate,
            design,
            glide: Glide::at_rest(&design, ramp_chunks(rate)),
            gain: F::ZERO,
            pole: F::ZERO,
            leave: F::ONE,
            into: 0,
            springs: vec![spring; usize::from(width.count())],
        };
        spring_reverb.apply();
        spring_reverb
    }

    /// The loop's gain at `controls`: below 1 whatever they are.
    #[must_use]
    pub fn loop_gain(&self, controls: &SpringControls) -> Feedback {
        Design::of(self.layout, self.rate, controls).loop_gain
    }

    /// Aims the glide at `controls`: the values move from where they are over the next 20 ms
    /// of chunks; the lines are kept. No allocation.
    pub fn retarget(&mut self, controls: &SpringControls) {
        self.design = Design::of(self.layout, self.rate, controls);
        self.glide.aim(&self.design);
    }

    /// Jumps to the targets now. No allocation.
    pub fn jump_to_target(&mut self) {
        self.glide.each().into_iter().for_each(Ramp::jump_to_target);
        self.apply();
    }

    /// Takes the glide's current values as the constants of the chunk.
    fn apply(&mut self) {
        let (low, high) = (self.glide.low.now(), self.glide.high.now());
        let pole = self.glide.pole.now();
        self.gain = F::from_design(self.glide.gain.now());
        self.pole = F::from_design(pole);
        self.leave = F::from_design(1.0 - pole);
        let (low, high) = (Feedback::new_clamped(low), Feedback::new_clamped(high));
        for spring in &mut self.springs {
            spring.low.iter_mut().for_each(|a| a.retarget(low));
            spring.high.iter_mut().for_each(|a| a.retarget(high));
        }
    }

    /// Back to silence, the chunks counted from here and the glide landed. No allocation.
    pub fn reset(&mut self) {
        for spring in &mut self.springs {
            spring.low.iter_mut().for_each(AllPassFilter::reset);
            spring.high.iter_mut().for_each(AllPassFilter::reset);
            spring.line.reset();
            spring.tone = F::ZERO;
        }
        self.into = 0;
        self.jump_to_target();
    }

    /// How long it rings after its input stops, at the longest decay and the lowest tone, to
    /// [`TAIL_FLOOR`], erring long.
    #[must_use]
    pub fn tail(&self) -> Tail {
        self.tail_of(&SpringControls {
            decay: SpringControls::MAX_DECAY,
            tone: SpringControls::MIN_TONE,
            dispersion: Mix::WET,
        })
    }

    /// How long it rings at `controls`, to [`TAIL_FLOOR`], erring long.
    #[must_use]
    pub fn tail_of(&self, controls: &SpringControls) -> Tail {
        Design::of(self.layout, self.rate, controls).tail()
    }

    /// Runs `input` into `output`, channel by channel. The glide takes a step at each chunk's
    /// start, so a call that ends inside a chunk goes on in the next. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.springs.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let half = F::lit(0.5);
        let mut at = 0;
        while at < frames {
            if self.into == 0 {
                self.glide.each().into_iter().for_each(|r| r.advance(1));
                self.apply();
            }
            let run = (CHUNK - self.into).min(frames - at);
            let (gain, pole, leave) = (self.gain, self.pole, self.leave);
            for (c, spring) in self.springs.iter_mut().enumerate().take(width) {
                let x = &input.plane(c).samples()[at..at + run];
                let mut plane = output.plane_mut(c);
                for (y, &x) in plane.samples_mut()[at..at + run].iter_mut().zip(x) {
                    // The sample pushed `N` frames ago, through the low-pass.
                    let back = spring.line.end().saturating_sub(Samples::new(1));
                    spring.tone = leave * spring.line.read_whole(back) + pole * spring.tone;
                    let v = x + gain * spring.tone;
                    let (mut low, mut high) = (v, v);
                    for a in &mut spring.low {
                        low = a.tick(low);
                    }
                    for a in &mut spring.high {
                        high = a.tick(high);
                    }
                    let out = half * (low + high);
                    spring.line.push_sample(out);
                    *y = out;
                }
            }
            self.into = (self.into + run) % CHUNK;
            at += run;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
