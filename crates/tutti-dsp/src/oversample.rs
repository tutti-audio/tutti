//! A linear-phase half-band oversampler: a cascade of 2x stages that takes a mono block up by
//! 2, 4 or 8, lets the caller process it at the higher rate, and brings it back down.
//!
//! # Planner and state
//!
//! [`Oversampling`] describes the oversampler (its [`OversamplingFactor`], the stopband
//! attenuation and the [`Passband`]) with no rate; [`plan`](Oversampling::plan) binds a
//! rate and designs the filters into an [`OversamplingPlan`], immutable and cheap to clone (the
//! taps sit behind an `Arc`). [`OversamplingPlan::state`] allocates one [`Oversampler`] per
//! channel, with its delay lines and the buffer of the oversampled signal. Both run on the
//! control side; [`Oversampler::upsample`] and [`Oversampler::downsample`] allocate nothing.
//!
//! # The cascade
//!
//! Stage `j` (counted from the base rate) runs at `base * 2^(j+1)` and is a Kaiser half-band
//! ([`FirResponse::HalfBand`]) cut at a quarter of its rate. Its transition is centred on the
//! previous stage's Nyquist, between the passband edge `p * base / 2` and its mirror: it is
//! `base * (2^j - p)` wide, so the first stage has the narrowest transition and the later ones,
//! which only reject images already far from the signal, are shorter. A transition wider than a
//! quarter of the stage's rate, `base * 2^(j-1)`, is cut to it: Kaiser's length estimate misses
//! its attenuation by several dB for a filter that short, and a narrower transition still
//! clears the same passband and stopband.
//!
//! A half-band of `N = 4m + 3` taps has a centre tap of 1/2 at offset `D = 2m + 1` and zeros at
//! every other even offset from it, so only the `2m + 2` outer taps multiply. Up, the input is
//! zero-stuffed: an output at an odd index is the centre tap alone (a pure delay by `D`, times
//! the 2 that undoes the zero-stuffing) and an output at an even index is `2 * sum(c[j] *
//! x[i - j])` over the outer taps. Down, an output is the same outer taps over every second
//! input frame plus the centre tap over the frame `D` back, and the filter is evaluated only at
//! the frames that survive decimation. Every stage has unity gain in its passband.
//!
//! # Latency
//!
//! A stage's delay is `D_j = (N_j - 1) / 2` frames of its own rate, and `D_j` is odd. The
//! round trip, at the oversampled rate `F = 2^s` for `s` stages, delays by
//!
//! ```text
//! T = 2 * sum over j < s of D_j * 2^(s - 1 - j)
//! ```
//!
//! frames, which is a whole number of base frames only when `F` divides it. The first stage's
//! delay is odd, so for `s >= 2` it does not. The last up stage therefore reads its input
//! `q / 2` frames late, a pure delay of `q = (-T) mod F` frames at the oversampled rate (`q` is even,
//! because `T` and `F` are, so the delay is whole frames of the last stage's input), and
//!
//! ```text
//! latency = (T + q) / F        base frames
//! ```
//!
//! is a whole number: [`OversamplingPlan::latency`]. A two-times oversampler needs no
//! padding, and its latency is the first stage's delay.
//!
//! # Gain and precision
//!
//! The filters and the oversampled signal run in the `F` of [`Oversampler`]. `f32` resolves
//! about -144 dB, past the default attenuation. The ripple of a passband the cascade crosses
//! twice is the sum of its stages' ripples, each about `10^(-attenuation / 20)`.

use std::sync::Arc;

use tutti_buffers::MaxBlock;
use tutti_buffers::{Float, Mono, MonoMut, Sample};
use tutti_units::{Db, Hz, SampleRate, SampleSpan, Samples};

use crate::{FirDesign, FirError, FirResponse};

/// The attenuation of an [`Oversampling`] until [`attenuation`](Oversampling::attenuation) sets
/// it: 100 dB puts the images of a tone 100 dB below it, past what a 16-bit signal resolves
/// and a few dB under what `f32` does, for about 70 taps in the first stage.
const DEFAULT_ATTENUATION: Db = Db::new(100.0);

tutti_units::ranged! {
    /// The top of the band the oversampler keeps flat, as a fraction of the base rate's
    /// Nyquist frequency: above 0 and below 1.
    ///
    /// [`new`](Self::new) refuses anything else and never clamps. The first stage's transition
    /// is `1 - p` of the base rate wide, so a value near 1 asks for a long filter.
    #[derive(PartialOrd)]
    Passband(f32), PassbandError, "a passband edge above 0 and below 1 of the base Nyquist",
    |raw| raw.is_finite() && raw > 0.0 && raw < 1.0
}

impl Passband {
    /// 0.9 of the base Nyquist: 19.8 kHz at 44.1 kHz and 21.6 kHz at 48 kHz, the audible band
    /// for a transition a tenth of the base rate wide.
    pub const DEFAULT: Self = Self(0.9);
}

impl Default for Passband {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How many times the base rate an [`Oversampler`] runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OversamplingFactor {
    /// Twice the base rate: one stage.
    X2,
    /// Four times: two stages.
    X4,
    /// Eight times: three stages.
    X8,
}

impl OversamplingFactor {
    /// The factor as a number: 2, 4 or 8.
    #[must_use]
    pub const fn get(self) -> usize {
        1 << self.stages()
    }

    /// The 2x stages in the cascade.
    const fn stages(self) -> usize {
        match self {
            Self::X2 => 1,
            Self::X4 => 2,
            Self::X8 => 3,
        }
    }
}

/// A half-band oversampler, described without a sample rate.
///
/// Built as `Oversampling::new(factor)` and the options [`attenuation`](Self::attenuation) and
/// [`passband`](Self::passband); [`plan`](Self::plan) binds the rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Oversampling {
    factor: OversamplingFactor,
    attenuation: Db,
    passband: Passband,
}

impl Oversampling {
    /// An oversampler by `factor`, at 100 dB of attenuation with a [`Passband`] of 0.9.
    #[must_use]
    pub const fn new(factor: OversamplingFactor) -> Self {
        Self {
            factor,
            attenuation: DEFAULT_ATTENUATION,
            passband: Passband::DEFAULT,
        }
    }

    /// The stopband attenuation of each half-band, in positive dB: what an image of a tone
    /// below the passband is attenuated by, and about the size of the passband's ripple
    /// (`10^(-attenuation / 20)`). [`plan`](Self::plan) refuses one that is not positive or
    /// past what the taps resolve.
    #[must_use]
    pub const fn attenuation(mut self, attenuation: Db) -> Self {
        self.attenuation = attenuation;
        self
    }

    /// The edge of the flat band.
    #[must_use]
    pub const fn passband(mut self, passband: Passband) -> Self {
        self.passband = passband;
        self
    }

    /// The factor.
    #[must_use]
    pub const fn factor(&self) -> OversamplingFactor {
        self.factor
    }

    /// Designs the cascade for signals at `rate`. Control side; allocates.
    ///
    /// # Errors
    ///
    /// The [`FirError`] of the half-band that cannot be designed: an attenuation that is not
    /// positive and finite or is past [`MAX_ATTENUATION`](crate::MAX_ATTENUATION), or a rate
    /// whose scaled rate overflows.
    pub fn plan(&self, rate: SampleRate) -> Result<OversamplingPlan, FirError> {
        let base = rate.get();
        let stages = self.factor.stages();
        // A rate that overflows when scaled is refused with this error.
        let overflow = FirError::NonPositiveTransition(Hz::new(f64::INFINITY));
        let high = SampleRate::new(base * self.factor.get() as f64).ok_or(overflow)?;
        let passband = f64::from(self.passband.get());
        let mut taps = Vec::with_capacity(stages);
        for j in 0..stages {
            let transition =
                base * (f64::from(1u32 << j) - passband).min(f64::from(1u32 << j) / 2.0);
            let stage_rate = SampleRate::new(base * f64::from(2u32 << j)).ok_or(overflow)?;
            let fir = FirDesign::new(FirResponse::HalfBand, Hz::new(transition))
                .attenuation(self.attenuation)
                .design(stage_rate)?;
            let delay = fir.delay().get();
            let outer: Box<[f64]> = fir.taps().step_by(2).collect();
            let centre = fir.taps().nth(delay).unwrap_or(0.5);
            taps.push(StageTaps {
                outer,
                centre,
                delay,
            });
        }
        let factor = self.factor.get();
        // The round trip's delay in frames of the oversampled rate, and the pad that makes it
        // whole base frames.
        let round_trip: usize = taps
            .iter()
            .enumerate()
            .map(|(j, s)| 2 * s.delay * (1 << (stages - 1 - j)))
            .sum();
        let pad = (factor - round_trip % factor) % factor;
        Ok(OversamplingPlan {
            shared: Arc::new(Plan {
                factor: self.factor,
                high,
                taps,
                lead: pad / 2,
                latency: Samples::new((round_trip + pad) / factor),
            }),
        })
    }
}

/// One half-band stage's taps.
#[derive(Clone, Debug)]
struct StageTaps<F = f64> {
    /// The `2m + 2` taps at even indices, from the first to the last tap of the filter. They
    /// include the centre's two neighbours; the centre itself is at an odd index. Symmetric.
    outer: Box<[F]>,
    /// The centre tap, 1/2.
    centre: F,
    /// `(N - 1) / 2`, odd.
    delay: usize,
}

#[derive(Debug)]
struct Plan {
    factor: OversamplingFactor,
    /// The base rate times the factor.
    high: SampleRate,
    taps: Vec<StageTaps>,
    /// The frames the last up stage reads its input late by.
    lead: usize,
    latency: Samples,
}

/// The designed cascade of an [`Oversampling`] at one rate: immutable, shared by every clone and
/// by every channel's [`Oversampler`].
#[derive(Clone, Debug)]
pub struct OversamplingPlan {
    shared: Arc<Plan>,
}

impl OversamplingPlan {
    /// The factor.
    #[must_use]
    pub fn factor(&self) -> OversamplingFactor {
        self.shared.factor
    }

    /// The rate of the oversampled signal: the base rate times the factor.
    #[must_use]
    pub fn high_rate(&self) -> SampleRate {
        self.shared.high
    }

    /// The round trip's delay in base-rate frames, up then down, a whole number (see the
    /// module docs).
    #[must_use]
    pub fn latency(&self) -> Samples {
        self.shared.latency
    }

    /// The group delay of the down-sampling half alone, in base frames: how far a generator
    /// that writes at the higher rate ([`Oversampler::source`]) is delayed. The half-bands are
    /// linear-phase, so every frequency in the passband is delayed by exactly this much; it is
    /// half the round trip before the latency's padding to whole frames.
    #[must_use]
    pub fn down_delay(&self) -> SampleSpan {
        SampleSpan::clamped(self.stage_delay() as f64 / self.shared.factor.get() as f64)
    }

    /// The summed stage delays, in frames of the higher rate.
    fn stage_delay(&self) -> usize {
        let stages = self.shared.taps.len();
        self.shared
            .taps
            .iter()
            .enumerate()
            .map(|(j, s)| s.delay << (stages - 1 - j))
            .sum()
    }

    /// The delay of the up-sampling half alone, in frames of the oversampled rate: the
    /// oversampled point `h` of an impulse that entered at base frame 0 peaks at `h` equal to
    /// this. It is the sum of the up stages' delays, each in oversampled frames, plus the
    /// pad that makes the round trip whole base frames (all of it on the up side), so it is
    /// not a whole number of base frames in general; the round trip is.
    pub(crate) fn upsample_delay(&self) -> usize {
        self.stage_delay() + 2 * self.shared.lead
    }

    /// One channel's state for blocks of at most `max_block` base frames, its filters and
    /// oversampled signal in `F`. Control side; allocates.
    #[must_use]
    pub fn state<F: Float>(&self, max_block: MaxBlock) -> Oversampler<F> {
        let stages = self.shared.taps.len();
        let block = max_block.get();
        let taps = self
            .shared
            .taps
            .iter()
            .map(|s| StageTaps {
                outer: s.outer.iter().map(|&t| F::from_design(t)).collect(),
                centre: F::from_design(s.centre),
                delay: s.delay,
            })
            .collect();
        let up = (0..stages)
            .map(|j| {
                let lead = if j + 1 == stages { self.shared.lead } else { 0 };
                let history = self.shared.taps[j].outer.len() - 1 + lead;
                Line::new(history, block << j)
            })
            .collect();
        let down = (0..stages)
            .map(|k| {
                let history = 2 * self.shared.taps[stages - 1 - k].delay;
                Line::new(history, block << (stages - k))
            })
            .collect();
        Oversampler {
            plan: self.clone(),
            taps,
            up,
            down,
            high: vec![F::ZERO; block * self.shared.factor.get()],
            frames: 0,
            max_block: block,
        }
    }
}

/// A stage's input: `history` frames of what came before, then the block.
#[derive(Clone, Debug)]
struct Line<F> {
    buf: Vec<F>,
    history: usize,
}

impl<F: Float> Line<F> {
    fn new(history: usize, max_input: usize) -> Self {
        Self {
            buf: vec![F::ZERO; history + max_input],
            history,
        }
    }

    /// Where the block of `len` frames goes.
    fn input_mut(&mut self, len: usize) -> &mut [F] {
        &mut self.buf[self.history..self.history + len]
    }

    /// Keeps the last `history` frames of the history and the `len` frames of block.
    fn advance(&mut self, len: usize) {
        self.buf.copy_within(len..len + self.history, 0);
    }
}

/// `line`'s `n_in` frames up by two into `out`, whose `2 * n_in` frames are the filtered,
/// zero-stuffed signal: `out[2i]` is twice the outer taps over the input, `out[2i + 1]` the
/// centre tap's delayed input, twice.
fn up_stage<F: Float>(taps: &StageTaps<F>, line: &mut Line<F>, n_in: usize, out: &mut [F]) {
    let width = taps.outer.len();
    for i in 0..n_in {
        let window = &line.buf[i..i + width];
        let even: F = taps.outer.iter().zip(window).map(|(&c, &x)| c * x).sum();
        out[2 * i] = even + even;
        let centre = taps.centre * line.buf[i + width / 2];
        out[2 * i + 1] = centre + centre;
    }
    line.advance(n_in);
}

/// `line`'s `2 * n_out` frames down by two into `out`: the half-band evaluated at the frames
/// that survive.
fn down_stage<F: Float, O: Float>(
    taps: &StageTaps<F>,
    line: &mut Line<F>,
    n_out: usize,
    out: &mut [O],
) {
    let delay = taps.delay;
    for (i, o) in out.iter_mut().enumerate().take(n_out) {
        let window = line.buf[2 * i..].iter().step_by(2);
        let outer: F = taps.outer.iter().zip(window).map(|(&c, &x)| c * x).sum();
        *o = (outer + taps.centre * line.buf[2 * i + delay]).convert_to::<O>();
    }
    line.advance(2 * n_out);
}

/// One channel's oversampler: the delay lines of every stage and the buffers of the oversampled
/// signal, for blocks of at most the `max_block` of [`OversamplingPlan::state`]. `F` is the
/// format the filters and the oversampled signal run in.
///
/// A block goes through [`upsample`](Self::upsample), is processed at the higher rate in
/// [`high_rate`](Self::high_rate) (read from the first view, written to the second), and comes
/// back through [`downsample`](Self::downsample).
#[derive(Clone, Debug)]
pub struct Oversampler<F: Sample = f32> {
    plan: OversamplingPlan,
    /// The plan's taps in `F`.
    taps: Vec<StageTaps<F>>,
    /// Stage `j`, from the base rate up.
    up: Vec<Line<F>>,
    /// Stage `s - 1 - k`, from the top rate down.
    down: Vec<Line<F>>,
    /// The up-sampled block.
    high: Vec<F>,
    /// The base frames of the block in flight.
    frames: usize,
    max_block: usize,
}

impl<F: Float> Oversampler<F> {
    /// The plan this state runs.
    #[must_use]
    pub fn plan(&self) -> &OversamplingPlan {
        &self.plan
    }

    /// The round trip's delay in base frames; see [`OversamplingPlan::latency`].
    #[must_use]
    pub fn latency(&self) -> Samples {
        self.plan.latency()
    }

    /// Back to silence: every delay line is zero. No allocation.
    pub fn reset(&mut self) {
        for line in self.up.iter_mut().chain(&mut self.down) {
            line.buf.fill(F::ZERO);
        }
        self.frames = 0;
    }

    /// Up-samples `input` into [`high_rate`](Self::high_rate)'s first view, which is then
    /// `input.frames() * factor` frames. Frames past the prepared maximum are ignored. No
    /// allocation.
    pub fn upsample<S: Float>(&mut self, input: Mono<'_, S>) {
        let n = input.frames().get().min(self.max_block);
        self.frames = n;
        for (line, x) in self.up[0].input_mut(n).iter_mut().zip(input) {
            *line = x.convert_to::<F>();
        }
        let stages = self.up.len();
        for j in 0..stages {
            let n_in = n << j;
            let taps = &self.taps[j];
            if j + 1 < stages {
                let (done, next) = self.up.split_at_mut(j + 1);
                up_stage(taps, &mut done[j], n_in, next[0].input_mut(2 * n_in));
            } else {
                up_stage(taps, &mut self.up[j], n_in, &mut self.high[..2 * n_in]);
            }
        }
    }

    /// Opens a block of `frames` base frames for a generator that has no input to up-sample:
    /// returns the higher-rate view (`frames * factor` frames) to write, which
    /// [`downsample`](Self::downsample) then reads. Frames past the prepared maximum are
    /// ignored. No allocation.
    pub fn source(&mut self, frames: Samples) -> MonoMut<'_, F> {
        self.frames = frames.get().min(self.max_block);
        self.high_rate().1
    }

    /// The block [`upsample`](Self::upsample) took, at the higher rate, and where its processed
    /// form is written: `frames * factor` frames each. The second view is what
    /// [`downsample`](Self::downsample) reads, so a pass-through copies the first into it.
    pub fn high_rate(&mut self) -> (Mono<'_, F>, MonoMut<'_, F>) {
        let n = self.frames << self.up.len();
        (
            Mono::new(&self.high[..n]),
            MonoMut::new(self.down[0].input_mut(n)),
        )
    }

    /// Down-samples what was written to [`high_rate`](Self::high_rate)'s second view into
    /// `output`, the same number of base frames as the block that was up-sampled. Frames past
    /// the prepared maximum are not written. No allocation.
    pub fn downsample<S: Float>(&mut self, mut output: MonoMut<'_, S>) {
        let n = output.frames().get().min(self.max_block);
        let out = &mut output.as_mut()[..n];
        let stages = self.down.len();
        for k in 0..stages {
            let n_out = n << (stages - 1 - k);
            let taps = &self.taps[stages - 1 - k];
            if k + 1 < stages {
                let (done, next) = self.down.split_at_mut(k + 1);
                down_stage(taps, &mut done[k], n_out, next[0].input_mut(n_out));
            } else {
                down_stage(taps, &mut self.down[k], n_out, out);
            }
        }
    }
}

#[cfg(test)]
mod tests;
