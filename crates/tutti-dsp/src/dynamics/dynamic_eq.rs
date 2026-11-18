//! [`DynamicEqBank`]: one EQ band whose gain follows the level of the signal in that band.
//!
//! The band is an SVF bell ([`SvfType::Bell`]) at a centre and a Q. A detector listens to the
//! **key** (the side-chain bus or the signal itself, as the caller hands the bank its
//! [`KeySource`](super::KeySource)) through a band-pass at the same centre and Q, so it
//! measures what the bell acts on. The detected level goes through a static curve of the
//! compressor's kind (threshold, knee, ratio, [`compute_compressor_gain_reduction`](crate::compute_compressor_gain_reduction)) into a
//! gain change in dB, held within the range, which an attack and a release smooth:
//!
//! - [`Downward`](DynamicMode::Downward): the band is cut by the reduction a compressor would
//!   apply to a level over the threshold, so it is flat until the band is loud.
//! - [`Upward`](DynamicMode::Upward): the band is boosted by the same curve mirrored about the
//!   threshold (`upward_gain_lift`), so it is flat until the band is quiet.
//!
//! # The detector
//!
//! The key through a band-pass is an alternating signal, so the detector reads its
//! **mean-square level**: a one-pole mean of the key's square, whose square root times
//! `sqrt(2)` is the amplitude of a sine (a sine at the centre reads its own amplitude
//! exactly, the band-pass's peak gain of `Q` divided out). The mean's time constant is
//! 8 periods of the centre, at most 25 ms: long enough that the
//! ripple of a sine's square (a fraction `1 / sqrt(1 + (4 pi tau f)^2)` of its mean) is under
//! 2% of the level at the centre, short enough to follow a band's own ring. The attack and
//! release then smooth the gain change on top.
//!
//! # The gain
//!
//! The gain is **linked**: one gain from the loudest key channel, applied to every channel.
//! It is designed once per [`COEFF_INTERVAL`] frames of the stream (a design is a `tan` and a
//! power), from the smoothed gain change as the previous period ended, and the bell's
//! coefficients glide to it over the period with the filter's state kept, so the gain the
//! signal hears trails the detector by up to two periods and does not click. The grid is the
//! bank's own frame count, so the render does not depend on how the stream is cut into blocks.
//!
//! The bank has no latency (the detector is on the signal and the key, never ahead of them).
//! Its tail is the bell's ring-out, which follows the Q.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::ChannelLayout;
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Samples, Seconds};

use super::curves::{gain_reduction, level_db, upward_gain_lift};
use super::key_filter::KeyStage;
use super::shared::{MAX_KNEE, floored};
use crate::finite::finite_or;
use crate::follower::Follower;
use crate::{
    COEFF_INTERVAL, Glide, Lanes, Lerp, SvfBank, SvfCoefficients, SvfCoeffs, SvfControls, SvfModel,
    SvfType,
};

/// The periods of the band's centre the detector's mean-square level is taken over: 8.
const DETECTOR_PERIODS: f64 = 8.0;

/// The longest time constant of the detector's mean, 25 ms: a low band does not wait longer.
const DETECTOR_MAX: Seconds = Seconds(0.025);

/// The largest threshold, in dB either way, a band runs: the upward curve mirrors a level about
/// it (`2 t - x`), which stays finite for `|t|` this size and a level at or over -144 dB.
const MAX_THRESHOLD: Db = Db(1.0e30);

/// The loudest key sample the detector squares: 1e15 (300 dB), whose square is finite.
const MAX_KEY: f32 = 1.0e15;

/// Which way a dynamic band moves from flat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DynamicMode {
    /// The band is cut when its level is over the threshold: the compressor's curve, as a
    /// negative gain.
    #[default]
    Downward,
    /// The band is boosted when its level is under the threshold: the compressor's curve
    /// mirrored about the threshold, as a positive gain.
    Upward,
}

/// The controls of a [`DynamicEqBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicEqControls {
    /// The centre of the band, of the bell and of the detector's band-pass. Held in the
    /// SVF's range.
    pub center: Hz,
    /// The Q of the band, of the bell and of the detector's band-pass. Held in the SVF's range.
    pub q: Q,
    /// The level, in dB, at which the band starts to move: the level over it for
    /// [`Downward`](DynamicMode::Downward), under it for [`Upward`](DynamicMode::Upward).
    pub threshold: Db,
    /// The width of the soft knee in dB, centred on the threshold; 0 is a hard knee. Held in
    /// `0..=400`, as the compressor's.
    pub knee: Db,
    /// dB in per dB out past the threshold, as the compressor's: at 4 a level 8 dB past it
    /// (or under it, upward) moves the band 6 dB. Below 1 reads as 1.
    pub ratio: CompressionRatio,
    /// How fast the gain change grows toward a larger one: the time constant of a one-pole
    /// smoother. Below 0 or NaN reads as 0 (instant); `+inf` holds the gain change.
    pub attack: Seconds,
    /// How fast the gain change falls back toward flat. Read as `attack` is.
    pub release: Seconds,
    /// The most the band's gain moves from flat, in dB and not below 0: the cut of a
    /// downward band, the boost of an upward one. Held in `0..=SvfCoeffs::MAX_GAIN`.
    pub range: Db,
}

impl DynamicEqControls {
    /// A band that does nothing: 1 kHz, Butterworth Q, threshold 0 dB, a hard knee, no ratio,
    /// no times and a range of 12 dB. What a non-finite control other than a time falls back to
    /// (a NaN time is 0 and an infinite one holds).
    pub const DEFAULT: Self = Self {
        center: Hz(1_000.0),
        q: Q::BUTTERWORTH,
        threshold: Db::UNITY,
        knee: Db::UNITY,
        ratio: CompressionRatio::UNITY,
        attack: Seconds::ZERO,
        release: Seconds::ZERO,
        range: Db(12.0),
    };
}

impl Default for DynamicEqControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The controls made runnable: the sanitised figures the detector and the curve read.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Design<F> {
    threshold: Db,
    knee: Db,
    ratio: CompressionRatio,
    range: Db,
    ballistics: Follower<F>,
    /// What the key's level is multiplied by: the band-pass's peak gain `Q` divided out.
    key_scale: F,
    /// The one-pole coefficient of the detector's mean square.
    mean: F,
}

impl<F: Float> Design<F> {
    const NEUTRAL: Self = Self {
        threshold: Db::UNITY,
        knee: Db::UNITY,
        ratio: CompressionRatio::UNITY,
        range: Db::UNITY,
        ballistics: Follower::NEUTRAL,
        key_scale: F::ONE,
        mean: F::ONE,
    };

    /// The design of `c` at `rate`, with `center` and `q` as the filters are given them.
    fn of(c: &DynamicEqControls, center: Hz, q: Q, rate: SampleRate) -> Self {
        let d = DynamicEqControls::DEFAULT;
        // A centre is held in the SVF's range, so it has a period; none reads as the longest time.
        let tau = center.period().map_or(DETECTOR_MAX, |period| {
            (period * DETECTOR_PERIODS).min(DETECTOR_MAX)
        });
        Self {
            threshold: Db(finite_or(c.threshold.get(), d.threshold.get()))
                .clamp(-MAX_THRESHOLD, MAX_THRESHOLD),
            knee: Db(finite_or(c.knee.get(), d.knee.get()).clamp(0.0, MAX_KNEE.get())),
            ratio: CompressionRatio::new_clamped(finite_or(c.ratio.get(), d.ratio.get())),
            range: Db(finite_or(c.range.get(), d.range.get()).clamp(0.0, SvfCoeffs::MAX_GAIN.get())),
            ballistics: Follower::of(floored(c.attack, c.release), rate),
            key_scale: F::ONE / F::from_unit(q),
            mean: F::from_f64(tau.one_pole_coeff(rate)),
        }
    }
}

/// The frames of the key a period reads at a time: the bell's design period.
const PERIOD: Samples = Samples(COEFF_INTERVAL);

/// One EQ band of any width whose bell gain follows the level of its key in the band.
///
/// Built on the control side for one rate; the state is `F`, the graph's float, in the detector
/// and the bell, and a view of another sample format is converted at the edges. Nothing it holds
/// grows.
#[derive(Clone, Debug)]
pub struct DynamicEqBank<F: Float + Lanes> {
    mode: DynamicMode,
    design: Design<F>,
    /// The centre and Q the filters are at, held in the SVF's range.
    center: Hz,
    q: Q,
    rate: SampleRate,
    /// The detector's band-pass on the key.
    key: KeyStage<F>,
    /// The mean square of the key, in the detector's one-pole: a per-sample value in `F`.
    mean_square: F,
    /// The smoothed gain change's size in dB, not below 0; its sign is the mode's: a per-sample
    /// level in `F`; becomes `Db` once that unit stores `f64`.
    moved: F,
    bell: SvfBank<F>,
    coefficients: Glide<SvfCoefficients<F>>,
    /// The gain change the coefficients were last designed at, in dB, `None` after the band
    /// moved.
    designed: Option<Db>,
    /// The frames into the current design period.
    phase: Samples,
}

impl<F: Float + Lanes + Lerp> DynamicEqBank<F> {
    /// A band of `mode` over `width` channels at `rate` that does nothing, to be aimed.
    /// Allocates: build it on the control side.
    #[must_use]
    pub fn new(mode: DynamicMode, width: ChannelLayout, rate: SampleRate) -> Self {
        let d = DynamicEqControls::DEFAULT;
        Self {
            mode,
            design: Design::NEUTRAL,
            center: d.center,
            q: d.q,
            rate,
            key: KeyStage::new(SvfType::BandPass, width, rate),
            mean_square: F::ZERO,
            moved: F::ZERO,
            bell: SvfBank::new(width),
            coefficients: Glide::landed(Self::bell(d.center, d.q, Db::UNITY, rate))
                .by(COEFF_INTERVAL as u32),
            designed: None,
            phase: Samples::ZERO,
        }
    }

    /// The bell at `center` and `q` with `gain` dB.
    fn bell(center: Hz, q: Q, gain: Db, rate: SampleRate) -> SvfCoefficients<F> {
        let controls = SvfControls {
            ty: SvfType::Bell,
            freq: center,
            q,
            gain,
        };
        SvfCoefficients::from(&SvfModel::new(controls, rate))
    }

    /// The gain change the band is at, in dB: negative for a cut, positive for a boost, 0
    /// flat. A **measurement**, for a meter. No allocation.
    #[must_use]
    pub fn gain(&self) -> Db {
        self.signed()
    }

    /// The gain change as a `Db`: the state widens once, here.
    fn signed(&self) -> Db {
        let moved = Db(self.moved.to_f64());
        match self.mode {
            DynamicMode::Downward => -moved,
            DynamicMode::Upward => moved,
        }
    }

    /// Back to silence: flat, nothing measured, the filters' state cleared. No allocation.
    pub fn reset(&mut self) {
        self.mean_square = F::ZERO;
        self.moved = F::ZERO;
        self.phase = Samples::ZERO;
        self.bell.reset();
        self.key.reset();
        self.settle();
    }

    /// Lands the filters on their design now.
    pub fn settle(&mut self) {
        self.design_bell();
        self.coefficients.settle();
        self.key.settle();
    }

    /// Aims at `controls`: the curve and the times land whole, and the bell and the detector's
    /// band-pass glide to the centre and Q over one grid period ([`COEFF_INTERVAL`]). A
    /// non-finite control is the default's, except a time (NaN is 0, `+inf` holds); a knee,
    /// the times and the range are floored at 0, a ratio at 1 and a threshold held within
    /// 1e30 dB. No allocation.
    pub fn aim(&mut self, controls: &DynamicEqControls) {
        let d = DynamicEqControls::DEFAULT;
        let model = SvfModel::new(
            SvfControls {
                ty: SvfType::Bell,
                freq: Hz(finite_or(controls.center.get(), d.center.get())),
                q: Q(finite_or(controls.q.get(), d.q.get())),
                gain: Db::UNITY,
            },
            self.rate,
        )
        .controls();
        self.center = model.freq;
        self.q = model.q;
        self.design = Design::of(controls, self.center, self.q, self.rate);
        self.key.aim(self.center, self.q);
        self.designed = None;
    }

    /// Aims the bell's glide at the band's gain now, if it is not already there.
    fn design_bell(&mut self) {
        let gain = self.signed();
        if self.designed != Some(gain) {
            self.coefficients
                .aim(Self::bell(self.center, self.q, gain, self.rate));
            self.designed = Some(gain);
        }
    }

    /// One frame of the detector: the key's `level` (an amplitude, the band-pass's peak gain
    /// not yet divided out) through the mean square, the static curve and the follower. The
    /// curve's controls widen to `F` here, exactly.
    #[inline]
    fn frame(&mut self, level: F) {
        let d = &self.design;
        let x = (level * d.key_scale).min(<F as Float>::from_f32(MAX_KEY));
        self.mean_square += d.mean * (x * x - self.mean_square);
        let db = level_db((<F as Float>::from_f32(2.0) * self.mean_square).sqrt());
        let (threshold, ratio, knee) = (
            F::from_f64(d.threshold.get()),
            F::from_unit(d.ratio),
            F::from_f64(d.knee.get()),
        );
        let curve = match self.mode {
            DynamicMode::Downward => gain_reduction(db, threshold, ratio, knee),
            DynamicMode::Upward => upward_gain_lift(db, threshold, ratio, knee),
        };
        let range = F::from_f64(d.range.get());
        self.moved = d.ballistics.follow_raw(self.moved, curve.min(range));
    }

    /// Runs `input` into `output`, its band moved by the gain the loudest channel of `key`
    /// sets.
    ///
    /// `key` is the signal the detector listens to: the side-chain bus, or `input` itself for
    /// a band on its own signal, always through the band-pass at the centre and Q (the signal
    /// the gain is applied to is never filtered but by the bell). Channel `c` of `input` feeds
    /// channel `c` of `output`; a channel past the narrower of the two views, or past the
    /// bank's width, is not touched, and the frames rendered are the fewest of the three
    /// views'. A NaN key sample reads as quiet and an infinite one as the largest finite
    /// level. No allocation.
    pub fn process<S: Float>(
        &mut self,
        // The key is `S` until the dynamics banks run in one float (R1b).
        key: Planes<'_, S>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input
            .frames()
            .get()
            .min(output.frames().get())
            .min(key.frames().get());
        let mut levels = [F::ZERO; PERIOD.get()];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(PERIOD.get() - self.phase.get());
            let end = start + n;
            if self.phase == Samples::ZERO {
                self.design_bell();
            }
            let levels = &mut levels[..n];
            self.key.levels(&key, start..end, levels);
            for &level in levels.iter() {
                self.frame(level);
            }
            let span = Samples(start)..Samples(end);
            self.bell.process(
                &mut self.coefficients,
                input.slice(span.clone()),
                output.slice_mut(span),
            );
            self.phase = (self.phase + Samples(n)) % PERIOD;
            start = end;
        }
    }
}

#[cfg(test)]
mod tests;
