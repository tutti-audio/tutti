//! Inter-channel phase correlation.
//!
//! The measurement a mastering engineer watches to catch mono-compatibility
//! problems: `+1` is identical channels, `0` uncorrelated, `-1` polarity
//! inverted and cancelling.
//!
//! Split into a pure kernel ([`correlate`]) and an explicit smoother
//! ([`StereoSmoother`]). Folding the two together invites a `width` stored
//! beside the correlation it is defined from and smoothed independently of it,
//! so the smoothed pair contradicts the invariant the kernel guarantees.

use tutti_buffers::{Sample, StereoPlanes};
use tutti_dsp::smooth_toward;
use tutti_units::{Amplitude, Ballistics, Correlation, Db, Seconds, StereoBalance, StereoWidth};

/// Mid/side and per-channel levels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StereoLevels {
    /// RMS [`Amplitude`] of the mid (sum) component.
    pub mid: Amplitude,
    /// RMS [`Amplitude`] of the side (difference) component.
    pub side: Amplitude,
    /// RMS [`Amplitude`] of the left channel.
    pub left: Amplitude,
    /// RMS [`Amplitude`] of the right channel.
    pub right: Amplitude,
}

impl StereoLevels {
    /// Mid-to-side ratio in dB. Positive is more mid — a narrower image.
    ///
    /// Clamped to ±60 dB and never infinite: a function whose name promises dB
    /// must not hand a meter an infinity to propagate.
    pub fn ms_ratio(&self) -> Db {
        // A meter range, deliberately not `Db::FLOOR`: this is a ratio between
        // two amplitudes on a ±60 dB scale, not a level pinned at the noise
        // floor. `Db::from_amplitude` leaves the floor to its consumer, and
        // this consumer's floor is the `-60` below, applied by the clamp.
        //
        // The three early returns cover every non-positive input, so the
        // conversion never sees one — the clamp bounds the finite range, not
        // the silent case.
        const FLOOR: Db = Db::new(-60.0);
        const CEILING: Db = Db::new(60.0);

        if self.mid.get() <= 0.0 && self.side.get() <= 0.0 {
            return Db::new(0.0);
        }
        if self.side.get() <= 0.0 {
            return CEILING;
        }
        if self.mid.get() <= 0.0 {
            return FLOOR;
        }
        Db::from_amplitude(Amplitude::new(self.mid.get() / self.side.get())).clamp(FLOOR, CEILING)
    }
}

/// One correlation reading.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StereoReading {
    /// Inter-channel phase [`Correlation`], `-1..=1`: `+1` identical channels,
    /// `0` uncorrelated, `-1` polarity inverted. A measurement, deliberately
    /// not `Bipolar` despite the coinciding range.
    pub correlation: Correlation,
    /// Where the energy sits on the left/right axis.
    pub balance: StereoBalance,
    /// Mid/side and per-channel RMS levels for this block.
    pub levels: StereoLevels,
}

impl StereoReading {
    /// The image width this correlation implies.
    ///
    /// Derived, not stored — a stored field can be smoothed independently of
    /// the correlation, letting the pair violate `width == 1 - correlation`.
    #[inline]
    pub fn width(&self) -> StereoWidth {
        self.correlation.to_stereo_width()
    }

    /// Whether the correlation is negative enough that a mono fold would
    /// cancel audibly.
    #[inline]
    #[must_use]
    pub fn has_phase_issues(&self) -> bool {
        self.correlation.has_phase_issues()
    }

    /// Whether the channels are near-identical (correlation above 0.95).
    #[inline]
    #[must_use]
    pub fn is_mono(&self) -> bool {
        self.correlation > Correlation::new(0.95)
    }
}

/// Correlates one block of stereo audio. Stateless.
///
/// Takes a [`StereoPlanes`] rather than two loose slices: mid/side and L/R
/// correlation are only defined at exactly two channels, and every statistic
/// below divides by a single frame count. Reconciling the pair here with
/// `left.len().min(right.len())` silently measures the shorter of two
/// mismatched blocks; the pairing cannot be formed at all unless they agree.
pub fn correlate<S: Sample>(planes: StereoPlanes<'_, S>) -> StereoReading {
    let n = planes.frames().get();
    if n == 0 {
        return StereoReading::default();
    }
    let (left, right) = (planes.left(), planes.right());

    // Un-centred f64 accumulators over the whole block: the correlation is
    // `Σlr / sqrt(Σll·Σrr)`, so no mean is removed and nothing cancels.
    let (mut sum_ll, mut sum_rr, mut sum_lr) = (0.0f64, 0.0, 0.0);
    let (mut sum_mid, mut sum_side) = (0.0f64, 0.0f64);

    for i in 0..n {
        let (l, r) = (left[i].convert_to::<f64>(), right[i].convert_to::<f64>());
        sum_ll += l * l;
        sum_rr += r * r;
        sum_lr += l * r;

        let mid = (l + r) * 0.5;
        let side = (l - r) * 0.5;
        sum_mid += mid * mid;
        sum_side += side * side;
    }

    let count = n as f64;
    let rms = |sum: f64| Amplitude::new((sum / count).sqrt());

    let denominator = (sum_ll * sum_rr).sqrt();
    let correlation = if denominator > 1e-20 {
        Correlation::new_clamped(sum_lr / denominator)
    } else {
        Correlation::UNCORRELATED
    };

    let (level_l, level_r) = (rms(sum_ll), rms(sum_rr));
    let total = level_l.get() + level_r.get();
    let balance = if total > 1e-20 {
        StereoBalance::new_clamped((level_r.get() - level_l.get()) / total)
    } else {
        StereoBalance::CENTER
    };

    StereoReading {
        correlation,
        balance,
        levels: StereoLevels {
            mid: rms(sum_mid),
            side: rms(sum_side),
            left: level_l,
            right: level_r,
        },
    }
}

/// A display filter over stereo level readings, not a stream meter: it takes a
/// reading and the time since the last, and carries the smoothed reading
/// between pushes with the attack and release of its [`Ballistics`].
#[derive(Debug, Clone, Copy)]
pub struct StereoSmoother {
    cfg: Ballistics,
    current: StereoReading,
}

impl StereoSmoother {
    /// A smoother of `ballistics`, seeded with a default (silent, uncorrelated) reading.
    #[must_use]
    pub fn new(ballistics: Ballistics) -> Self {
        Self {
            cfg: ballistics,
            current: StereoReading::default(),
        }
    }

    /// A smoother with 10 ms attack and 100 ms release, the standard meter ballistics.
    ///
    /// # Panics
    ///
    /// Never: 10 ms and 100 ms are finite and not negative, which `Ballistics::new` accepts.
    #[must_use]
    pub fn standard() -> Self {
        Self::new(
            Ballistics::new(Seconds::new(0.01), Seconds::new(0.1))
                .expect("10 ms and 100 ms are valid times"),
        )
    }

    /// The latest smoothed reading.
    #[inline]
    #[must_use]
    pub fn current(&self) -> StereoReading {
        self.current
    }

    /// Discards the smoothed reading, so the next push starts from silence
    /// rather than decaying from the previous signal.
    pub fn reset(&mut self) {
        self.current = StereoReading::default();
    }

    /// Smooths an instantaneous reading, `dt` after the last, and returns the
    /// result.
    ///
    /// Returns the *smoothed* value, which is the useful one: returning the
    /// instantaneous reading instead and hiding the smoothed one behind a
    /// separate `current()` call makes the obvious use of the return value the
    /// wrong one.
    ///
    /// Only `correlation` and the levels are smoothed; width is derived from the
    /// smoothed correlation afterwards, so the two cannot disagree.
    pub fn push(&mut self, instant: StereoReading, dt: Seconds) -> StereoReading {
        let cfg = self.cfg;
        let smooth = |old: f64, new: f64| smooth_toward(old, new, cfg.coeff_over(new > old, dt));
        let smooth_level = |old: Amplitude, new: Amplitude| {
            Amplitude::new(smooth_toward(
                old.get(),
                new.get(),
                cfg.coeff_over(new > old, dt),
            ))
        };

        let correlation = Correlation::new_clamped(smooth(
            self.current.correlation.get(),
            instant.correlation.get(),
        ));
        let balance =
            StereoBalance::new_clamped(smooth(self.current.balance.get(), instant.balance.get()));

        let levels = StereoLevels {
            mid: smooth_level(self.current.levels.mid, instant.levels.mid),
            side: smooth_level(self.current.levels.side, instant.levels.side),
            left: smooth_level(self.current.levels.left, instant.levels.left),
            right: smooth_level(self.current.levels.right, instant.levels.right),
        };

        self.current = StereoReading {
            correlation,
            balance,
            levels,
        };
        self.current
    }
}

#[cfg(test)]
mod tests;
