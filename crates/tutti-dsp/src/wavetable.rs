//! The wavetable oscillator: single-cycle frames, mipmapped by FFT bin zeroing, read by a
//! running phase through cubic Hermite interpolation.
//!
//! # The table
//!
//! A [`Wavetable`] is built on the control side from one or more frames of `N` samples each
//! ([`TableSize`], a power of two), and shared in an `Arc` by every [`WavetableOscillator`]
//! that plays it. Each frame is stored at `L = log2(N) - 1` levels. Level `j` keeps the DC bin
//! and the harmonics `1..=K_j` with `K_j = N / 2^(j+2)` of the frame's DFT and zeroes the bins
//! above, by an inverse FFT of the cut spectrum (run in `f64`, narrowed once to `F`). Level 0
//! is a table with a factor two of oversampling over its harmonics; the top level is the
//! fundamental alone.
//!
//! # Level selection
//!
//! Level `j` played at `f` puts its top harmonic at `K_j f`, so it is free of aliasing while
//! `f <= f_j = fs / (2 K_j)`, which is `fs 2^(j+1) / N`. A frequency `f` is read from two
//! adjacent levels `m` and `m + 1`, crossfaded linearly in `log2 f`, where `m` is the least
//! level with `f <= f_m` and the weight of level `m + 1` is `log2(f / f_{m-1})` clamped to
//! `0..=1`: the fade runs over the octave below `f_m`, so level `m + 1` plays alone from `f_m`
//! up, and level `m` is never played above its own limit. Below `f_0 / 2` level 0 plays
//! alone, and the top level is alias-free up to Nyquist. A frequency outside `0..=Nyquist`
//! is clamped for the step.
//!
//! The crossfade changes the pitch of nothing and moves no phase, so a sweep through a level
//! boundary is continuous: the output is a convex blend of two band-limited tables read at one
//! phase.
//!
//! # Interpolation and its bound
//!
//! A table is read by Catmull-Rom cubic Hermite interpolation ([`cubic_hermite`]) at the
//! phase, taken exactly from the running phase's count. For a harmonic `h` of amplitude `a`,
//! sampled at `N` points per cycle with `w = 2πh/N`, the interpolation error is at most
//! `a E(h)` with `E(h) = (4/81) w^3 + w^4 / 384`: the Hermite basis maxima `4/27` twice, times
//! the central-difference tangent error `w^3 / 6`, plus the Hermite remainder `w^4 / 384`.
//! Level selection puts no kept harmonic above Nyquist, so this error ([`interpolation_error`](Wavetable::interpolation_error)),
//! summed over a level's harmonics, is the whole
//! deviation of the output from the band-limited table's exact value, aliasing included, and
//! bounds the amplitude of any single partial in it.
//!
//! # The frame position
//!
//! A table of `M` frames is read at a [`Normalized`] position `p`: frame `p (M - 1)`, the two
//! frames either side of it crossfaded linearly by the fractional part. Position `i / (M - 1)`
//! plays frame `i` alone, exactly.

use std::sync::Arc;
use tutti_buffers::{Float, Mono, MonoMut};
use tutti_units::{Hz, Normalized, PhaseIncrement, PhaseStep, RunningPhase, SampleRate};

use crate::fft::{Complex, RealFft};
use crate::{cubic_hermite, lerp};

tutti_units::ranged! {
    /// The samples in one frame of a [`Wavetable`]: a power of two from 16 to 65536.
    TableSize(usize), TableSizeError, "a power of two from 16 to 65536",
    |raw| raw.is_power_of_two() && raw >= 16 && raw <= 65_536
}

/// Why a [`Wavetable`] cannot be built from some frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WavetableError {
    /// No frames were given.
    #[error("a wavetable has at least one frame")]
    NoFrames,
    /// The first frame's length is not a [`TableSize`].
    #[error("the frame length is not a table size: {0}")]
    Size(#[from] TableSizeError),
    /// A frame is not as long as the first.
    #[error("frame {frame} is not as long as frame 0")]
    Mismatch {
        /// The index of the frame.
        frame: usize,
    },
    /// A frame holds a NaN or an infinity.
    #[error("frame {frame} holds a sample that is not finite")]
    NotFinite {
        /// The index of the frame.
        frame: usize,
    },
}

/// Single-cycle frames and their mipmap levels; see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct Wavetable<F: Float> {
    size: TableSize,
    frames: usize,
    levels: usize,
    /// `[frame][level][sample]`, each `size` samples.
    data: Vec<F>,
}

impl<F: Float> Wavetable<F> {
    /// The table of `frames`, each a single cycle of `N` samples, in an `Arc` to share.
    /// Allocates; control side.
    ///
    /// # Errors
    /// [`WavetableError`] for no frames, a first frame whose length is not a [`TableSize`],
    /// a frame of another length, or a sample that is not finite.
    pub fn new(frames: &[Mono<'_, F>]) -> Result<Arc<Self>, WavetableError> {
        let first = frames.first().ok_or(WavetableError::NoFrames)?;
        let size = TableSize::try_from(first.samples().len())?;
        let n = size.get();
        let levels = n.trailing_zeros() as usize - 1;
        let fft = RealFft::<f64>::new(n);
        let mut scratch = fft.new_scratch();
        let mut x = vec![0.0f64; n];
        let mut bins = vec![Complex::new(0.0, 0.0); fft.bins()];
        let mut cut = bins.clone();
        let mut data = Vec::with_capacity(frames.len() * levels * n);
        for (frame, samples) in frames.iter().enumerate() {
            let samples = samples.samples();
            if samples.len() != n {
                return Err(WavetableError::Mismatch { frame });
            }
            for (d, &s) in x.iter_mut().zip(samples) {
                *d = s.into();
                if !d.is_finite() {
                    return Err(WavetableError::NotFinite { frame });
                }
            }
            fft.forward(Mono::new(&x), &mut bins, &mut scratch);
            for level in 0..levels {
                let keep = harmonics(n, level);
                for (k, (c, b)) in cut.iter_mut().zip(&bins).enumerate() {
                    *c = if k <= keep {
                        *b
                    } else {
                        Complex::new(0.0, 0.0)
                    };
                }
                fft.inverse(&mut cut, MonoMut::new(&mut x), &mut scratch);
                data.extend(x.iter().map(|&v| F::from_design(v / n as f64)));
            }
        }
        Ok(Arc::new(Self {
            size,
            frames: frames.len(),
            levels,
            data,
        }))
    }

    /// The samples in a frame.
    #[must_use]
    pub fn size(&self) -> TableSize {
        self.size
    }

    /// The frames.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The mipmap levels of each frame, `log2(size) - 1`.
    #[must_use]
    pub fn levels(&self) -> usize {
        self.levels
    }

    /// The harmonics `level` keeps: `size / 2^(level + 2)`.
    ///
    /// # Panics
    /// If `level` is not below [`levels`](Self::levels).
    #[must_use]
    pub fn harmonics(&self, level: usize) -> usize {
        assert!(level < self.levels, "BUG: a level of the table");
        harmonics(self.size.get(), level)
    }

    /// One level of one frame, `size` samples.
    ///
    /// # Panics
    /// If `frame` or `level` is out of range.
    #[must_use]
    pub fn level(&self, frame: usize, level: usize) -> Mono<'_, F> {
        assert!(
            frame < self.frames && level < self.levels,
            "BUG: a frame and level of the table"
        );
        let n = self.size.get();
        let start = (frame * self.levels + level) * n;
        Mono::new(&self.data[start..start + n])
    }

    /// The interpolation error of reading a harmonic of unit amplitude: `E(h)` of the
    /// module docs. A harmonic of amplitude `a` is read within `a E(h)`.
    #[must_use]
    pub fn interpolation_error(&self, harmonic: usize) -> f64 {
        let w = core::f64::consts::TAU * harmonic as f64 / self.size.get() as f64;
        4.0 / 81.0 * w * w * w + w * w * w * w / 384.0
    }

    /// The sample of `level` of `frame` at the running phase `phase`: cubic Hermite between
    /// the four neighbours.
    #[inline]
    fn read(&self, frame: usize, level: usize, phase: RunningPhase) -> F {
        let n = self.size.get();
        let bits = n.trailing_zeros();
        let count = phase.get();
        let i = (count >> (64 - bits)) as usize;
        let t = F::from_design((count << bits) as f64 * (1.0 / 18_446_744_073_709_551_616.0));
        let start = (frame * self.levels + level) * n;
        let at = |k: usize| self.data[start + (i.wrapping_add(k) & (n - 1))];
        cubic_hermite(at(n - 1), at(0), at(1), at(2), t)
    }
}

/// The harmonics level `level` of a table of `size` samples keeps.
fn harmonics(size: usize, level: usize) -> usize {
    size >> (level + 2)
}

/// One sample's advance of a [`WavetableOscillator`]: the phase step, the pair of levels and
/// their weight, and the pair of frames and theirs, for one frequency and position.
///
/// Built when the frequency or the position changes; the level and the frames are chosen
/// here, so a block that holds both builds one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WavetableStep<F: Float> {
    step: PhaseStep,
    level: usize,
    level_weight: F,
    frame: usize,
    frame_weight: F,
}

impl<F: Float> WavetableStep<F> {
    /// The step of `frequency` at `rate` over `table`, at `position` of its frames. The
    /// frequency is clamped to `0..=Nyquist` and one that is not finite holds the phase.
    #[must_use]
    pub fn new(
        table: &Wavetable<F>,
        frequency: Hz,
        rate: SampleRate,
        position: Normalized,
    ) -> Self {
        let f = frequency.finite_or(Hz::ZERO).get();
        let cycles = (f / rate.get()).clamp(0.0, 0.5);
        let n = table.size.get() as f64;
        let top = table.levels - 1;
        // `log2(f / f_0) + 1`, with `f_j = fs 2^(j + 1) / n`: level `m` is the least with
        // `f <= f_m`, and `log2(f / f_{m-1}) = log2 r - m`.
        let (level, level_weight) = if cycles > 0.0 {
            let r = libm::log2(cycles * n);
            let m = libm::ceil(r - 1.0).max(0.0) as usize;
            if m >= top {
                (top, 0.0)
            } else {
                (m, (r - m as f64).clamp(0.0, 1.0))
            }
        } else {
            (0, 0.0)
        };
        let x = position.get() * (table.frames - 1) as f64;
        let frame = (x as usize).min(table.frames - 1);
        let frame_weight = if frame + 1 < table.frames {
            x - frame as f64
        } else {
            0.0
        };
        Self {
            step: PhaseStep::from(PhaseIncrement::new(cycles)),
            level,
            level_weight: F::from_design(level_weight),
            frame,
            frame_weight: F::from_design(frame_weight),
        }
    }

    /// The lower of the two levels read, and the weight of the next one above it, `0..=1`.
    #[must_use]
    pub fn level(&self) -> (usize, F) {
        (self.level, self.level_weight)
    }

    /// The lower of the two frames read, and the weight of the next one above it, `0..=1`.
    #[must_use]
    pub fn frame(&self) -> (usize, F) {
        (self.frame, self.frame_weight)
    }
}

/// A scalar wavetable oscillator: a running phase reading a shared [`Wavetable`] at a
/// frequency that may change every sample, given as a [`WavetableStep`].
///
/// # Examples
///
/// ```
/// use tutti_buffers::Mono;
/// use tutti_dsp::{Wavetable, WavetableOscillator, WavetableStep};
/// use tutti_units::{Hz, Normalized, SampleRate};
///
/// let frame: Vec<f64> = (0..64)
///     .map(|i| (core::f64::consts::TAU * i as f64 / 64.0).sin())
///     .collect();
/// let table = Wavetable::new(&[Mono::new(&frame)]).unwrap();
/// let step = WavetableStep::new(&table, Hz::new(12_000.0), SampleRate::SR_48K, Normalized::of(0.0));
/// let mut osc = WavetableOscillator::new(table);
/// assert!(osc.step(step).abs() < 1e-9);
/// // A quarter of a cycle later: the peak.
/// assert!((osc.step(step) - 1.0).abs() < 1e-6);
/// ```
#[derive(Clone, Debug)]
pub struct WavetableOscillator<F: Float> {
    table: Arc<Wavetable<F>>,
    phase: RunningPhase,
}

impl<F: Float> WavetableOscillator<F> {
    /// An oscillator of `table` at phase 0.
    #[must_use]
    pub fn new(table: Arc<Wavetable<F>>) -> Self {
        Self {
            table,
            phase: RunningPhase::START,
        }
    }

    /// The table it reads.
    #[must_use]
    pub fn table(&self) -> &Arc<Wavetable<F>> {
        &self.table
    }

    /// Returns the phase to 0.
    pub fn reset(&mut self) {
        self.phase = RunningPhase::START;
    }

    /// The sample at the current phase, then the phase advanced by `step`. `step` is built
    /// over this oscillator's table.
    ///
    /// # Panics
    /// If `step` names a level or frame the table does not have.
    #[inline]
    pub fn step(&mut self, step: WavetableStep<F>) -> F {
        let phase = self.phase;
        let table = &*self.table;
        let on_level = |level: usize| {
            let a = table.read(step.frame, level, phase);
            if step.frame_weight == F::ZERO {
                a
            } else {
                lerp(
                    a,
                    table.read(step.frame + 1, level, phase),
                    step.frame_weight,
                )
            }
        };
        let low = on_level(step.level);
        let y = if step.level_weight == F::ZERO {
            low
        } else {
            lerp(low, on_level(step.level + 1), step.level_weight)
        };
        self.phase = phase.advance(step.step);
        y
    }
}

#[cfg(test)]
mod tests;
