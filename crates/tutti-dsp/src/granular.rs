//! Granular synthesis: [`Granular`], a cloud of Hann-windowed grains read from a shared
//! source at fractional positions, started by a [`GrainScheduler`].

use std::sync::Arc;

use tutti_buffers::{Float, PlanesMut, Wave};
use tutti_rt::SplitMix64;
use tutti_units::{
    Bipolar, Cents, Hz, Normalized, PlaybackRate, ReadPosition, ReadRate, SampleRate, SampleSpan,
    Samples, Seconds, SrcRatio,
};

use crate::cubic_hermite;

tutti_units::ranged! {
    /// The most grains a [`Granular`] sounds at once: `1..=1024`.
    MaxGrains(usize), MaxGrainsError, "a grain count from 1 to 1024",
    |raw| raw >= 1 && raw <= 1024
}

/// How a cloud of grains is made: density, length, source position, pitch and jitter.
///
/// Read when a grain starts, so a change is heard from the next grain.
///
/// ```
/// use tutti_dsp::GrainSettings;
/// use tutti_units::{Cents, Hz, Normalized, Seconds};
///
/// let settings = GrainSettings::new(Hz::new(20.0), Seconds::new(0.08))
///     .position(Normalized::of(0.3))
///     .rate(Cents::new(700.0))
///     .position_jitter(Normalized::of(0.05))
///     .timing_jitter(Normalized::of(0.25));
/// # let _ = settings;
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrainSettings {
    density: Hz,
    length: Seconds,
    position: Normalized,
    rate: Cents,
    position_jitter: Normalized,
    timing_jitter: Normalized,
}

impl GrainSettings {
    /// Grains of `length` started `density` times a second, from the start of the source, at
    /// the source's own pitch, with no jitter. A density that is not above zero starts none.
    #[must_use]
    pub const fn new(density: Hz, length: Seconds) -> Self {
        Self {
            density,
            length,
            position: Normalized::of(0.0),
            rate: Cents::new(0.0),
            position_jitter: Normalized::of(0.0),
            timing_jitter: Normalized::of(0.0),
        }
    }

    /// Where in the source a grain starts, before jitter: `0` its first frame and `1` its end.
    #[must_use]
    pub const fn position(mut self, position: Normalized) -> Self {
        self.position = position;
        self
    }

    /// The pitch a grain is read at, against the source's own.
    #[must_use]
    pub const fn rate(mut self, rate: Cents) -> Self {
        self.rate = rate;
        self
    }

    /// How far a grain's start may move from its position, to either side, as a fraction of
    /// the source: uniform in `+-spread`, then kept within the source.
    #[must_use]
    pub const fn position_jitter(mut self, spread: Normalized) -> Self {
        self.position_jitter = spread;
        self
    }

    /// How far each interval between onsets may move, as a fraction of the interval: uniform
    /// in `+-fraction`, so the mean density is unchanged.
    #[must_use]
    pub const fn timing_jitter(mut self, fraction: Normalized) -> Self {
        self.timing_jitter = fraction;
        self
    }
}

/// One grain's start: the output frame it begins at and where in the source it reads from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Onset {
    /// The output frame the grain's first sample is at.
    pub frame: Samples,
    /// Where the grain's first sample is in the source: `0` its first frame, `1` its end.
    pub position: Normalized,
}

/// The places and times of grains: seeded, so the same seed and settings give the same
/// [`Onset`]s.
///
/// Onset `k` of a run is at the whole frame of `k * rate / density` after the run's first,
/// computed from the index, never summed: it is exact however long the run, and an onset
/// whose time is a whole number of frames is at that frame (`7 * 48000 / 7` is 48000, not a
/// frame short). The timing jitter moves each onset by `+-fraction` of an interval from its own
/// nominal time, not from the last onset, so the mean density is unchanged and the jitter does
/// not accumulate. A change of density starts a new run from the last onset's nominal time.
///
/// Each onset draws two words, one for its timing and one for its position, whatever the
/// settings' values.
#[derive(Clone, Debug)]
pub struct GrainScheduler {
    seed: u64,
    rng: SplitMix64,
    /// The nominal time of the run's onset 0, in output frames from the start.
    base: SampleSpan,
    /// The index of the last onset in the run.
    index: u64,
    /// The density of the run.
    density: Hz,
}

impl GrainScheduler {
    /// A scheduler of the stream `seed` names.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            seed,
            rng: SplitMix64::new(seed),
            base: SampleSpan::ZERO,
            index: 0,
            density: Hz::new(0.0),
        }
    }

    /// Back to the start of the stream.
    pub fn reset(&mut self) {
        *self = Self::new(self.seed);
    }

    /// A uniform draw in `[-1, 1)`.
    fn draw(&mut self) -> Bipolar {
        Bipolar::clamped(2.0 * f64::from_fraction_bits(self.rng.next_u64()) - 1.0)
    }

    /// The nominal time of onset `index` of a run from `base` at `density`: `index * rate /
    /// density` frames on. The product of the integer index and the rate is exact below `2^53`
    /// and the division rounds once, so a quotient that is a whole number is exact.
    fn nominal(base: SampleSpan, index: u64, density: Hz, rate: SampleRate) -> SampleSpan {
        base + SampleSpan::clamped(index as f64 * rate.get() / density.get())
    }

    /// The onset at the run's current index, its timing moved by `settings`' jitter.
    fn onset(&mut self, settings: &GrainSettings, rate: SampleRate) -> Onset {
        let nominal = Self::nominal(self.base, self.index, self.density, rate);
        let (timing, place) = (self.draw().get(), self.draw().get());
        let interval = Self::nominal(SampleSpan::ZERO, 1, self.density, rate);
        let moved = interval
            .scaled(settings.timing_jitter.get() * timing.abs())
            .unwrap_or(SampleSpan::ZERO);
        let time = if timing < 0.0 {
            nominal.saturating_sub(moved)
        } else {
            nominal + moved
        };
        Onset {
            frame: time.to_samples_floor(),
            position: Normalized::clamped(
                settings.position.get() + settings.position_jitter.get() * place,
            ),
        }
    }

    /// Whether `settings` has a density a run can have.
    fn valid(settings: &GrainSettings) -> bool {
        settings.density.get() > 0.0 && settings.density.get().is_finite()
    }

    /// The onset at frame `at`, which a new run follows from; `None` when the density is not
    /// above zero.
    pub fn restart(
        &mut self,
        at: Samples,
        settings: &GrainSettings,
        rate: SampleRate,
    ) -> Option<Onset> {
        if !Self::valid(settings) {
            return None;
        }
        self.base = SampleSpan::from_samples(at);
        self.index = 0;
        self.density = settings.density;
        Some(self.onset(settings, rate))
    }

    /// The onset after the last one. A density that differs from the run's starts a new run
    /// from the last onset's nominal time. `None` when the density is not above zero.
    pub fn next(&mut self, settings: &GrainSettings, rate: SampleRate) -> Option<Onset> {
        if !Self::valid(settings) {
            return None;
        }
        if settings.density != self.density {
            self.base = Self::nominal(self.base, self.index, self.density, rate);
            self.index = 0;
            self.density = settings.density;
        }
        self.index += 1;
        Some(self.onset(settings, rate))
    }
}

/// How often a grain's window phasor is set exactly again: every `RESEED` frames.
const RESEED: usize = 64;

/// A grain in flight.
///
/// Its window is the periodic Hann `(1 - c) / 2` of the phasor `(c, s) = (cos, sin)` of
/// `2 pi age / length`, turned by one frame's angle each frame, `c' = c cos(d) - s sin(d)` and
/// `s' = s cos(d) + c sin(d)`, and set to the exact `cos` and `sin` at the grain's start and
/// every [`RESEED`] frames. Each turn adds at most `4 eps` of error to `c` (the two rounded
/// coefficients, the products and the sums), so `c` is within `4 RESEED eps` of the closed
/// form whatever the grain's length: `4 * 64 * 2^-23 = 3.1e-5` at `f32` and `5.7e-14` at `f64`.
#[derive(Clone, Copy, Debug)]
struct Grain<F: Float> {
    /// The source frame the first output frame reads.
    start: SampleSpan,
    /// Source frames read per output frame.
    rate: ReadRate,
    /// Output frames played.
    age: Samples,
    /// Output frames in the grain, at least 1 and at most `u32::MAX`.
    length: Samples,
    /// The window phasor.
    cos: F,
    sin: F,
    /// One frame's turn of the phasor.
    turn_cos: F,
    turn_sin: F,
}

impl<F: Float> Grain<F> {
    fn new(start: SampleSpan, rate: ReadRate, length: Samples) -> Self {
        let turn = F::from_design(core::f64::consts::TAU / SampleSpan::from_samples(length).get());
        Self {
            start,
            rate,
            age: Samples::ZERO,
            length,
            cos: F::ONE,
            sin: F::ZERO,
            turn_cos: turn.cos(),
            turn_sin: turn.sin(),
        }
    }

    /// The window at this frame.
    fn window(&self) -> F {
        F::lit(0.5) * (F::ONE - self.cos)
    }

    /// Where this frame reads in the source: the start plus the age times the read rate,
    /// from the age each time, so the read position does not drift.
    fn position(&self) -> ReadPosition {
        let advance = SampleSpan::from_samples(self.age)
            .scaled(self.rate.get())
            .unwrap_or(SampleSpan::ZERO);
        ReadPosition::from_span(self.start + advance)
    }

    /// To the next frame.
    fn advance(&mut self) {
        self.age += Samples::new(1);
        if self.age.get().is_multiple_of(RESEED) {
            let angle = core::f64::consts::TAU
                * (SampleSpan::from_samples(self.age) / SampleSpan::from_samples(self.length));
            let angle = F::from_design(angle);
            (self.cos, self.sin) = (angle.cos(), angle.sin());
        } else {
            (self.cos, self.sin) = (
                self.cos * self.turn_cos - self.sin * self.turn_sin,
                self.sin * self.turn_cos + self.cos * self.turn_sin,
            );
        }
    }

    fn done(&self) -> bool {
        self.age >= self.length
    }
}

/// A granular engine over an `Arc`-shared source.
///
/// A grain plays `length` of the source from a start position, at a pitch ratio
/// `2^(cents / 1200)` against the source's own rate (within [`PlaybackRate`]'s `0.25..=4`, so
/// `+-2400` cents), under a periodic Hann window `w[n] = (1 - cos(2 pi n / N)) / 2` of its `N`
/// frames, computed by a phasor that is set exactly every 64 frames: within `2 eps` of the
/// closed form, `eps` the float's epsilon, times 64. The window peaks at 1 and a grain has
/// no gain of its own, so a cloud of density `d` and length `L` sums to about `d L` times the
/// source's level; periodic Hann grains started every `N / 2` frames sum to exactly 1.
///
/// A grain reads the source through [`cubic_hermite`] at a [`ReadPosition`] (whole frames and
/// a fraction of `2^-32`), a step per output frame of `ratio * source rate / output rate`.
/// The cubic is chosen over the resampler's windowed sinc because a cloud reads many grains
/// per output frame: it costs four taps a channel, and its error on a tone of `omega` radians
/// per source frame is within `omega^3 / 4`. It does not band-limit, so a grain read faster
/// than the source's own rate (a positive pitch) folds the source's content above
/// `Nyquist / ratio`. A tap before the first source frame or past the last is silence.
///
/// Grains start at integer output frames. The [`GrainScheduler`] places the onsets of a
/// density as a running time in fractional frames, each grain starting at the whole frame of
/// its onset, so the mean density is exact however the rate divides it. Position and timing
/// jitter are drawn from a seeded [`SplitMix64`]: the same seed and settings give the same
/// onsets and positions.
///
/// The grain slots are allocated once, by [`Granular::new`]. A grain whose onset finds every
/// slot busy is not started and is counted ([`Granular::dropped`]), never waited on.
///
/// A grain reads every source channel at the same position. A mono source feeds every output
/// plane; a wider one feeds the plane of its own index, and an output plane past its channels
/// is silent.
///
/// ```
/// use std::sync::Arc;
/// use tutti_buffers::{ChannelLayout, MonoMut, PlanesMut, Wave};
/// use tutti_dsp::{Granular, GrainSettings, MaxGrains};
/// use tutti_units::{Hz, SampleRate, Seconds};
///
/// let rate = SampleRate::SR_48K;
/// let mut source = Wave::<f32>::new(1, rate);
/// source.append(tutti_buffers::Interleaved::new(&[0.5; 4800], ChannelLayout::MONO)?)?;
/// let mut cloud = Granular::new(Arc::new(source), rate, MaxGrains::of(8), 1);
/// let settings = GrainSettings::new(Hz::new(100.0), Seconds::new(0.01));
/// let mut out = vec![vec![0.0_f32; 480]];
/// cloud.process(&settings, PlanesMut::from_vecs(&mut out)?);
/// assert!(out[0].iter().any(|&x| x > 0.4));
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone, Debug)]
pub struct Granular<F: Float> {
    source: Arc<Wave<F>>,
    rate: SampleRate,
    scheduler: GrainScheduler,
    /// The onset to start next, `None` until one is placed.
    pending: Option<Onset>,
    /// Output frames rendered since the start.
    now: Samples,
    slots: Vec<Option<Grain<F>>>,
    dropped: u64,
}

impl<F: Float> Granular<F> {
    /// A cloud of up to `max` grains over `source`, rendering at `rate`, drawing its jitter
    /// from the stream `seed` names. Allocates the grain slots; nothing after does.
    #[must_use]
    pub fn new(source: Arc<Wave<F>>, rate: SampleRate, max: MaxGrains, seed: u64) -> Self {
        Self {
            source,
            rate,
            scheduler: GrainScheduler::new(seed),
            pending: None,
            now: Samples::ZERO,
            slots: vec![None; max.get()],
            dropped: 0,
        }
    }

    /// The onsets that found every slot busy and were not started, since the start.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The grains sounding now.
    #[must_use]
    pub fn active(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Back to the start: no grain sounding, the stream reseeded, the count cleared. No
    /// allocation.
    pub fn reset(&mut self) {
        self.scheduler.reset();
        self.pending = None;
        self.now = Samples::ZERO;
        self.slots.fill(None);
        self.dropped = 0;
    }

    /// Starts the grain of `onset` in a free slot, or counts it dropped.
    fn start(&mut self, onset: Onset, settings: &GrainSettings) {
        let Some(slot) = self.slots.iter_mut().find(|s| s.is_none()) else {
            self.dropped += 1;
            return;
        };
        let source_rate = self.source.sample_rate();
        let at =
            SampleSpan::from_seconds(self.source.duration() * onset.position.get(), source_rate)
                .unwrap_or(SampleSpan::ZERO);
        let rate = PlaybackRate::clamped(settings.rate.to_pitch_ratio())
            .read_rate(SrcRatio::for_rates(source_rate, self.rate));
        let length = settings.length.to_samples(self.rate).get();
        let length = Samples::new(length.clamp(1, u32::MAX as usize));
        *slot = Some(Grain::new(at, rate, length));
    }

    /// Adds the sounding grains over `out`'s frames from `from`, `frames` of them.
    fn mix(&mut self, out: &mut PlanesMut<'_, '_, F>, from: usize, frames: usize) {
        let mono = self.source.channels() == 1;
        let channels = if mono {
            out.channels()
        } else {
            out.channels().min(self.source.channels())
        };
        for slot in &mut self.slots {
            let Some(grain) = slot else { continue };
            let run = frames.min(grain.length.remaining_after(grain.age).get());
            for n in 0..run {
                let window = grain.window();
                let position = grain.position();
                for c in 0..channels {
                    let tap = read(
                        self.source.channel(if mono { 0 } else { c }).samples(),
                        position,
                    );
                    out.plane_mut(c).samples_mut()[from + n] += window * tap;
                }
                grain.advance();
            }
            if grain.done() {
                *slot = None;
            }
        }
    }

    /// Renders the next frames of the cloud into `out`, overwriting it.
    ///
    /// Grains start at the frames of their onsets whatever the block's size. No allocation.
    pub fn process(&mut self, settings: &GrainSettings, mut out: PlanesMut<'_, '_, F>) {
        out.silence();
        let frames = out.frames().get();
        if self.pending.is_none() {
            self.pending = self.scheduler.restart(self.now, settings, self.rate);
        }
        let mut done = 0;
        while done < frames {
            // Start every grain due at this frame, then play up to the next onset or the end.
            let frame = self.now.get() + done;
            while let Some(onset) = self.pending.filter(|o| o.frame.get() <= frame) {
                self.start(onset, settings);
                self.pending = self.scheduler.next(settings, self.rate);
            }
            let until = self
                .pending
                .map_or(frames, |o| (o.frame.get() - self.now.get()).min(frames));
            self.mix(&mut out, done, until - done);
            done = until;
        }
        self.now = Samples::new(self.now.get() + frames);
    }
}

/// The source at `position`, by cubic Hermite interpolation; silence for a tap outside it.
fn read<F: Float>(source: &[F], position: ReadPosition) -> F {
    let (whole, fraction) = F::split_position(position);
    let i = whole.get();
    let tap = |k: usize| {
        // The tap at `i - 1 + k`.
        (i + k)
            .checked_sub(1)
            .and_then(|j| source.get(j))
            .copied()
            .unwrap_or(F::ZERO)
    };
    cubic_hermite(tap(0), tap(1), tap(2), tap(3), fraction)
}

#[cfg(test)]
mod tests;
