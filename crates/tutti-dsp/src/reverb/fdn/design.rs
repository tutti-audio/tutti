//! [`FdnLayout`] and [`FdnControls`]: what a feedback delay network is built to and told.
//!
//! `N` delay lines of mutually unrelated lengths (a table of values near primes,
//! scaled by the room size), each with a three-band absorption set from the
//! three RT60s (`Absorption::designed`: the loop gain that loses 60 dB in
//! that time), mixed by an orthogonal reflection `A = I - 2 v v^T`, with an
//! input pre-delay, a slow modulation of the line lengths, a stereo output
//! (even lines left, odd right) with a width and a wet gain, and, for the plain
//! [`FdnLayout`], a freeze ([`FreezeControls`]): every gain exactly one and
//! the input gated off, so the network is lossless and its tail never ends.
//!
//! **Diffusion** is the vector `v`: from one axis (no line feeds another,
//! parallel combs, the sparse end) to `1/sqrt(N)` on every line (the
//! Householder matrix, the smooth end), a unit vector all the way, so the loop
//! never gains whatever the setting. At 0 the network says
//! [`Matrix::Diagonal`], at 1 [`Matrix::Householder`] and in between
//! [`Matrix::Reflection`].
//!
//! Lengths are rounded to whole frames, so a settled network delays by whole
//! frames and a frozen one loses nothing to interpolation; a modulated line
//! reads between frames, and freezing turns the modulation off.
//!
//! The design is [`FdnLayout::design_into`]: the controls to a [`Network`], in place, since
//! a network is too large to build on an audio thread. [`FdnBank`](crate::FdnBank) runs it.
//! The shimmer's own layout and controls are in [`ShimmerLayout`](crate::ShimmerLayout).

use tutti_units::{
    Bipolar, Db, Gain, Hz, Mix, SampleRate, SampleSpan, Samples, Seconds, StereoWidth,
};

use super::kind::FreezeControls;
use crate::reverb::network::{
    Absorption, Fdn, Gains, Inline, Line, LineModulation, MAX_LINES, Matrix, Network,
    NetworkCapacity, NetworkOutputMap, NetworkStage, pre_delay_frames,
};
use crate::{MaxDecay, TAIL_FLOOR};

/// The line lengths at 44.1 kHz for a room size of 0.5 (the scale is
/// `0.5 + size`), values near primes, so the delays share few factors.
const LENGTHS: [SampleSpan; 16] = [
    SampleSpan::of(1031.0),
    SampleSpan::of(1327.0),
    SampleSpan::of(1523.0),
    SampleSpan::of(1801.0),
    SampleSpan::of(2053.0),
    SampleSpan::of(2287.0),
    SampleSpan::of(2621.0),
    SampleSpan::of(2999.0),
    SampleSpan::of(3331.0),
    SampleSpan::of(3571.0),
    SampleSpan::of(3911.0),
    SampleSpan::of(4241.0),
    SampleSpan::of(4549.0),
    SampleSpan::of(4793.0),
    SampleSpan::of(5099.0),
    SampleSpan::of(5393.0),
];

/// The crossovers of the three bands.
pub(crate) const CROSSOVER: [Hz; 2] = [Hz::new(300.0), Hz::new(4_000.0)];

/// The longest pre-delay, in seconds.
pub const MAX_PRE_DELAY: Seconds = Seconds::new(0.5);
/// The deepest modulation, in frames at 44.1 kHz, at a depth of 1.
const MAX_MODULATION: SampleSpan = SampleSpan::of(8.0);
/// The modulation rates a control may ask for.
const RATES: (Hz, Hz) = (Hz::new(0.05), Hz::new(5.0));
/// The shortest RT60 a control may ask for, and the least a [`MaxDecay`] holds.
pub const MIN_DECAY: Seconds = Seconds::new(0.05);
/// The widest stereo image.
const MAX_WIDTH: f64 = 2.0;
/// The wet gain at or below which the network is silent.
const SILENT_WET: Db = Db::new(-120.0);
/// The loudest wet gain.
const MAX_WET: Db = Db::new(24.0);

/// The controls every feedback delay network has, in tutti units. Each kind adds its own:
/// an [`FdnLayout`] takes [`FreezeControls`] and a
/// [`ShimmerLayout`](crate::ShimmerLayout) takes [`ShimmerControls`](crate::ShimmerControls).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdnControls {
    /// The RT60 of the mid band (index 0 of [`UnitParam::Decay`](tutti_units::UnitParam)),
    /// the low band (1) and the high band (2): `[mid, low, high]`.
    pub decay: [Seconds; 3],
    /// The room size, `0..1`: scales every line, from half to one and a half
    /// times the table. **A move lands over seconds, by design**: the realization
    /// moves a line half a frame per 32-frame chunk (a pitch bend of 27 cents),
    /// so the longest line going from size 0 to 1 takes about 6 s, to keep a
    /// size sweep free of pitch artefacts.
    pub size: Mix,
    /// How much the lines mix, `0..1`: `0` is parallel combs, `1` the smooth end.
    pub diffusion: Mix,
    /// The delay before the first sound.
    pub pre_delay: Seconds,
    /// The depth of the modulation of the line lengths, `0..1` (read as its
    /// absolute value).
    pub modulation: Bipolar,
    /// The rate of that modulation.
    pub rate: Hz,
    /// The stereo width of the wet signal: `0` is mono, `1` natural.
    pub width: StereoWidth,
    /// The wet gain.
    pub wet: Db,
}

impl FdnControls {
    /// A mid-sized room with a two second tail, darker above: the controls an
    /// FDN starts from.
    pub const DEFAULT: FdnControls = FdnControls {
        decay: [Seconds::new(2.0), Seconds::new(2.4), Seconds::new(1.2)],
        size: Mix::new(0.5),
        diffusion: Mix::new(0.8),
        pre_delay: Seconds::ZERO,
        modulation: Bipolar::of(0.3),
        rate: Hz::new(0.5),
        width: StereoWidth::NATURAL,
        wet: Db::UNITY,
    };
}

impl Default for FdnControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a plain feedback delay network is built to: its lines and its longest decay.
/// Mono in, stereo out. It can freeze (see [`FreezeControls`]), so its tail is unbounded over
/// every control. A network that shifts the pitch of its feedback is a
/// [`ShimmerLayout`](crate::ShimmerLayout).
///
/// A control outside its range is clamped and a non-finite one takes the
/// [`FdnControls::DEFAULT`] value, so every control gives a stable network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdnLayout {
    lines: usize,
    max_decay: Seconds,
}

impl FdnLayout {
    /// The lines of the default network.
    const LINES: usize = 8;

    /// A design of `lines` lines (2 to 16, even: half feed each ear), RT60s up
    /// to `max_decay`, which holds its tail on a freeze. `None` when `lines` is out of range
    /// (`max_decay` is range-checked by its type).
    #[must_use]
    pub fn new(lines: usize, max_decay: MaxDecay) -> Option<Self> {
        let ok = (2..=LENGTHS.len()).contains(&lines) && lines.is_multiple_of(2);
        ok.then_some(Self {
            lines,
            max_decay: max_decay.get(),
        })
    }

    /// The lines.
    #[must_use]
    pub fn lines(&self) -> usize {
        self.lines
    }
}

impl FdnLayout {
    /// What `NetworkCapacity::tail` is multiplied by for the rounding of the reflection
    /// vector. Each entry of a unit `v` is rounded twice (stored as `f32`, the narrowest width, then
    /// divided by the norm and rounded again), 2^-23 relative in all, so `|v|^2`
    /// is 1 to within `e = 2^-22` and `|A| = 1 + 2e` at most. `rt60_bound`
    /// applies that norm **every frame** (the step of its proof), where the decay
    /// is `3 / (RT60 fs)` decades a frame, so the bound reads a decay slower by
    /// `x = 2e / ln 10 * RT60 fs / 3` of itself, and the tail by `1 / (1 - x)`:
    /// 0.5% at 8 s and 44.1 kHz, 11% at 192 kHz.
    pub(crate) fn rounding_margin(&self, rate: SampleRate) -> f64 {
        let e = 2.0 * f64::from(f32::EPSILON);
        let x = 2.0 * e / std::f64::consts::LN_10 * self.max_decay.get() * rate.get() / 3.0;
        1.0 / (1.0 - x.min(0.5))
    }
}

impl Default for FdnLayout {
    /// Eight lines, RT60s up to 8 s.
    fn default() -> Self {
        Self {
            lines: Self::LINES,
            max_decay: MaxDecay::DEFAULT.get(),
        }
    }
}

impl FdnLayout {
    /// Designs `c` into `out` in place, at `rate`: the network the bank runs, and the public
    /// entry for designing one (for its analytic queries as much as for running it). A
    /// network is about 115 KB, so it is built where it lives and never returned by value; no
    /// allocation.
    pub fn design_into(
        &self,
        c: &FdnControls,
        freeze: &FreezeControls,
        rate: SampleRate,
        out: &mut Network,
    ) {
        self.design(c, freeze.freeze, rate, out);
    }

    /// [`design_into`](Self::design_into) with the freeze as a flag, for a kind that never
    /// freezes.
    pub(crate) fn design(
        &self,
        c: &FdnControls,
        freeze: bool,
        rate: SampleRate,
        out: &mut Network,
    ) {
        let d = FdnControls::DEFAULT;
        let rt60 = [
            c.decay[1], // low
            c.decay[0], // mid
            c.decay[2], // high
        ]
        .map(|s| {
            let v = s.finite_or(d.decay[0]);
            v.clamp(MIN_DECAY, self.max_decay)
        });
        let rt60 = if freeze {
            [Seconds::new(f64::INFINITY); 3]
        } else {
            rt60
        };
        let size = c.size.finite_or(d.size).get().clamp(0.0, 1.0);
        let diffusion = c.diffusion.finite_or(d.diffusion).get().clamp(0.0, 1.0);
        let pre = c
            .pre_delay
            .finite_or(Seconds::ZERO)
            .clamp(Seconds::ZERO, MAX_PRE_DELAY);
        let depth = c.modulation.get().abs().min(1.0);
        let mod_rate = c.rate.finite_or(d.rate).clamp(RATES.0, RATES.1);
        let width = c
            .width
            .finite_or(StereoWidth::NATURAL)
            .get()
            .clamp(0.0, MAX_WIDTH);
        let wet = {
            let w = c.wet;
            if w.get().is_nan() {
                Db::UNITY
            } else if w <= SILENT_WET {
                Db::new(f64::NEG_INFINITY)
            } else {
                w.min(MAX_WET)
            }
        };

        let n = self.lines;
        let scale = (0.5 + size) * (rate.get() / 44_100.0);
        let depth_frames = if freeze {
            SampleSpan::ZERO
        } else {
            // `depth` is in `0..=1` and the rate finite and positive: the product is a
            // finite span, and `clamped` saturates where a span could not hold it.
            SampleSpan::clamped(MAX_MODULATION.get() * (depth * rate.get() / 44_100.0))
        };
        let mut lines: Inline<Line, MAX_LINES> = Inline::new();
        for &base in &LENGTHS[..n] {
            // The lengths are whole frames: the product rounds to one.
            let length = SampleSpan::clamped((base.get() * scale).round().max(1.0));
            let _ = lines.push(Line {
                length,
                modulation: LineModulation {
                    depth: depth_frames,
                    rate: mod_rate,
                },
                absorption: Absorption::designed(rt60, CROSSOVER, length, rate),
                diffuser: None,
            });
        }

        // The reflection vector: one axis at diffusion 0, the flat vector at 1.
        let flat = 1.0 / (n as f64).sqrt();
        let mut v = Gains::filled(Gain::SILENT, n).unwrap_or_default();
        {
            let mut norm = 0.0f64;
            for (i, x) in v.as_mut_slice().iter_mut().enumerate() {
                let axis = if i == 0 { 1.0 } else { 0.0 };
                let y = crate::lerp(axis, flat, diffusion);
                *x = Gain::new(y);
                norm += y * y;
            }
            let norm = norm.sqrt();
            for x in v.as_mut_slice() {
                *x = Gain::new(x.get() / norm);
            }
        }
        let matrix = if diffusion <= 0.0 {
            Matrix::Diagonal
        } else if diffusion >= 1.0 {
            Matrix::Householder
        } else {
            Matrix::Reflection(v)
        };

        // One mono input into every line, none when frozen; even lines to the
        // left ear, odd to the right.
        let gain = (1.0 / (n as f64).sqrt()).min(1.0);
        let input =
            Gains::filled(Gain::new(if freeze { 0.0 } else { gain }), n).unwrap_or_default();
        let ear = (2.0 / n as f64).sqrt();
        let mut left = Gains::filled(Gain::SILENT, n).unwrap_or_default();
        let mut right = Gains::filled(Gain::SILENT, n).unwrap_or_default();
        for (i, (l, r)) in left
            .as_mut_slice()
            .iter_mut()
            .zip(right.as_mut_slice())
            .enumerate()
        {
            if i % 2 == 0 {
                *l = Gain::new(ear);
            } else {
                *r = Gain::new(ear);
            }
        }
        let mut rows = Inline::new();
        let _ = rows.push(left);
        let _ = rows.push(right);

        out.clear(rate);
        out.pre_delay = pre_delay_frames(pre, rate);
        let _ = out.stages.push(NetworkStage::Fdn(Fdn {
            lines,
            matrix,
            input,
            output: NetworkOutputMap { rows },
            loop_delay: SampleSpan::ZERO,
        }));
        out.width = StereoWidth::new(width);
        out.wet = wet;
    }
}

/// Sets the loop delay of `network`'s recirculating stages: the shifted path's delay, which the
/// decay queries read as a longer trip for every line.
pub(crate) fn set_loop_delay(network: &mut Network, delay: Samples) {
    for stage in network.stages.as_mut_slice() {
        if let NetworkStage::Fdn(f) = stage {
            f.loop_delay = SampleSpan::from_samples(delay);
        }
    }
}

impl FdnLayout {
    /// The worst case over every control: size 1, the longest decay in every band, the
    /// longest pre-delay, the deepest modulation, with `loop_delay` in every line's loop. The
    /// tail is the network's own at the longest decay. Control side.
    pub(crate) fn capacity(&self, rate: SampleRate, loop_delay: Samples) -> NetworkCapacity {
        let worst = FdnControls {
            decay: [self.max_decay; 3],
            size: Mix::new(1.0),
            diffusion: Mix::new(1.0),
            pre_delay: MAX_PRE_DELAY,
            modulation: Bipolar::of(1.0),
            ..FdnControls::DEFAULT
        };
        let mut network = Box::new(Network::new(rate));
        self.design(&worst, false, rate, &mut network);
        set_loop_delay(&mut network, loop_delay);
        NetworkCapacity::need(&network, TAIL_FLOOR)
    }
}

#[cfg(test)]
mod tests;
