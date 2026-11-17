//! A streaming spectrum: power in dB per bin or band, with smoothing, peak hold
//! and a rolling spectrogram.
//!
//! [`SpectrumMeter::push`] takes blocks of any length. Samples go into a ring of one
//! window, and a frame is analysed whenever a hop of new samples has arrived
//! since the last one, so the frames do not depend on how the stream is cut.
//! Frame `k` covers samples `k * hop .. k * hop + window` of everything fed
//! since the meter was made or [reset](SpectrumMeter::reset); the first is
//! ready once a whole window has arrived.
//!
//! # Levels
//!
//! A bin's level is the power of its amplitude, in dB, referenced to a sine of
//! amplitude 1: a sine of amplitude `a` centred on a bin reads `20 log10(a)`
//! there. With coherent window gain `g` over `N` samples, a bin `X` reads
//! `10 log10(4 |X|^2 / (g N)^2)`. The DC bin, and the Nyquist bin of an even
//! window, have no negative-frequency image to carry half the amplitude, so
//! they drop the factor 4: a DC offset `A` reads `20 log10(A)` and a Nyquist
//! tone of amplitude `A` reads `20 log10(A)`, as a sine would on any other
//! bin. Off the centre of a bin a
//! tone loses up to the window's scalloping loss (1.42 dB for Hann), and a
//! window of `N` samples spreads a tone over the bins its main lobe covers.
//!
//! The floor is absolute: a level below [`floor`](SpectrumConfig::floor), and
//! silence, reads the floor, never `-inf`. Power is clamped at +200 dB, so an
//! absurdly large but finite input cannot reach infinity or NaN.
//!
//! # Bands
//!
//! With [`Banding::Octave`] or [`Banding::Log`] each output is a band, and its
//! power is the sum of the power of the bins it holds: a band's level is a
//! power over its width, not a mean. A bin belongs to the band whose
//! `[lower, upper)` edges contain its frequency. The DC bin is never in a band
//! (the lowest edge is above 0), and the Nyquist bin is not when `max` is
//! Nyquist. A sine inside one band reads its level plus the window's noise
//! bandwidth in power: `10 log10(1.5)` = 1.76 dB for Hann, the sum of its
//! main bin and two half-amplitude neighbours.
//!
//! A band that holds no bin is refused when the state is built
//! ([`Error::EmptyBand`]); the bands cannot be finer than the bins. A band of
//! width `w` Hz is sure to hold a bin once the window is at least `rate / w`
//! samples, so the narrowest band, the lowest, sets the window a banding
//! needs: third-octave bands from 20 Hz (the first is 5.2 Hz wide) at 48 kHz
//! need 16384 samples. The error names the first empty band and a window that
//! holds every band.
//!
//! # Smoothing, peaks, spectrogram
//!
//! [`Smoothing`] works on the linear power of each output with
//! [`Ballistics`](tutti_types::Ballistics)'s one-pole step, so an averaged level is the dB of an
//! average power, and a released level falls linearly in dB per second. The
//! first frame seeds the state. The peak of an output is held for
//! [`hold`](SpectrumConfig::peak_hold) rounded up to a whole number of frames,
//! then falls by the decay in dB per second, never below the level. The
//! spectrogram keeps the newest columns of levels, oldest first.
//!
//! # Allocation
//!
//! [`SpectrumConfig::measure`] builds every buffer and plans the transform; a
//! push then allocates nothing, including after a
//! [`reset`](SpectrumMeter::reset) (`tests/alloc/spectrum.rs`).

use tutti_buffers::{Mono, Sample};
use tutti_types::{Ballistics, Db, Hz, SampleRate, Samples, Seconds};

use crate::chroma::FrequencyBand;
use crate::convert::{all_finite, for_blocks};
use crate::framing::{Framing, check_nyquist};
use crate::{Error, Result};
use tutti_dsp::Complex32;
use tutti_dsp::FftScratch;
use tutti_dsp::FrameCount;
use tutti_dsp::Smoother;
use tutti_dsp::StftGeometry;
use tutti_types::ranged;

/// The smallest and largest window accepted.
const MIN_WINDOW: usize = 64;
const MAX_WINDOW: usize = 1 << 16;

/// The most hops in one window; each frame is a whole transform, so this
/// bounds the cost of a tiny hop.
const MAX_OVERLAP: usize = 64;

/// The most outputs a banding may have.
const MAX_BANDS: usize = 4096;

/// The most cells (columns times outputs) a spectrogram may hold: 16 MiB of
/// `f32`.
const MAX_SPECTROGRAM_CELLS: usize = 1 << 22;

/// The sample rates accepted, in hertz.
const MIN_RATE: SampleRate = SampleRate::of(1.0);
const MAX_RATE: SampleRate = SampleRate::of(10_000_000.0);

/// Power clamp: +200 dB.
const MAX_POWER: f32 = 1e20;

/// Smoothed power under this is set to zero, so a release does not run the
/// float unit through subnormals.
const POWER_EPSILON: f32 = 1e-30;

ranged! {
    /// A time constant or a hold: within `0..=3600` seconds.
    SpectrumTime(Seconds), SpectrumTimeError, "a spectrum time within 0..=3600 s",
    |raw| raw.get() >= 0.0 && raw.get() <= 3600.0
}

ranged! {
    /// The level silence and anything quieter reads: within `-300..=0` dB.
    SpectrumFloor(Db), SpectrumFloorError, "a spectrum floor within -300..=0 dB",
    |raw| raw.get() >= -300.0 && raw.get() <= 0.0
}

ranged! {
    /// How fast a held peak falls, in [`Db`] per second: within `0..=10000`. No
    /// unit covers a rate of decibels.
    PeakDecay(Db), PeakDecayError, "a peak decay within 0..=10000 dB per second",
    |raw| raw.get() >= 0.0 && raw.get() <= 10_000.0
}

ranged! {
    /// Bands per octave: within `1..=48`.
    BandsPerOctave(usize), BandsPerOctaveError, "a band count per octave within 1..=48",
    |raw| raw >= 1 && raw <= 48
}

ranged! {
    /// The number of log-spaced bands: within `1..=4096`.
    BandCount(usize), BandCountError, "a band count within 1..=4096",
    |raw| raw >= 1 && raw <= 4096
}

/// How the per-output power is smoothed from frame to frame.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Smoothing {
    /// Each frame's power is the reading.
    None,
    /// Instant attack; the power falls toward a lower reading with this time
    /// constant.
    Release(SpectrumTime),
    /// The power moves toward each reading with this time constant, rising and
    /// falling alike: an exponential average over the time constant.
    Average(SpectrumTime),
}

impl Smoothing {
    fn ballistics(self) -> Ballistics {
        let (attack, release) = match self {
            Self::None => (Seconds(0.0), Seconds(0.0)),
            Self::Release(tau) => (Seconds(0.0), tau.get()),
            Self::Average(tau) => (tau.get(), tau.get()),
        };
        Ballistics::new(attack, release).expect("a `SpectrumTime` is within 0..=3600 s")
    }
}

/// How the bins of a frame are grouped into outputs.
///
/// A banding's lowest band is its narrowest, and it must hold a bin: the
/// window must be at least `rate / width` samples for a first band of `width`
/// Hz, else [`SpectrumConfig::measure`] refuses with [`Error::EmptyBand`].
/// Bands do not merge or skip: a banding either has every band it names or is
/// refused. The DC bin is in no band, and the Nyquist bin is in none when the
/// band's top is Nyquist. A band reads the summed power of its bins, so a sine inside one
/// reads about 1.76 dB over its amplitude with a Hann window.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Banding {
    /// One output per bin, `DC..=Nyquist`.
    Bins,
    /// `per_octave` bands per octave, band `j` spanning
    /// `lower * 2^(j / per_octave) .. lower * 2^((j + 1) / per_octave)`, where
    /// `lower` is the band's lower edge, for as many bands as end at or below
    /// its upper edge (3 per octave is third-octave).
    Octave {
        /// Bands per octave.
        per_octave: BandsPerOctave,
        /// The lower edge of the first band, and the frequency the last band
        /// ends at or below.
        band: FrequencyBand,
    },
    /// `count` bands of equal ratio from the band's lower edge to its upper edge.
    Log {
        /// The number of bands.
        count: BandCount,
        /// The lower edge of the first band, and the upper edge of the last.
        band: FrequencyBand,
    },
}

/// What a spectrum analyses with: the framing, the banding, the smoothing and
/// the peak and spectrogram settings. Immutable once built, and free of a
/// sample rate: [`SpectrumConfig::measure`] takes it, derives the geometry and
/// checks every setting that depends on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectrumConfig {
    framing: Framing,
    banding: Banding,
    smoothing: Smoothing,
    floor: SpectrumFloor,
    peak_hold: SpectrumTime,
    peak_decay: PeakDecay,
    columns: usize,
}

impl SpectrumConfig {
    /// A spectrum over frames of 2048 samples every 512: one level per bin, no
    /// smoothing, a floor of [`Db::FLOOR`], peaks held for a second then
    /// falling 20 dB per second, and no spectrogram.
    #[must_use]
    pub fn new() -> Self {
        Self {
            framing: Framing::new(Samples(2048), Samples(512)),
            banding: Banding::Bins,
            smoothing: Smoothing::None,
            floor: SpectrumFloor::of(Db::FLOOR),
            peak_hold: SpectrumTime::of(Seconds(1.0)),
            peak_decay: PeakDecay::of(Db(20.0)),
            columns: 0,
        }
    }

    /// The window and hop frames are taken at.
    ///
    /// [`measure`](Self::measure) refuses a window under 64 or over 65536
    /// samples ([`Error::WindowOutOfRange`]), a hop wider than the window
    /// ([`Error::HopWiderThanWindow`]) and a hop giving more than 64 frames
    /// per window ([`Error::TooManyOverlaps`]).
    #[must_use]
    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    /// Groups the bins into bands.
    ///
    /// [`measure`](Self::measure) refuses a banding the framing cannot hold:
    /// more than 4096 bands ([`Error::TooManyBands`]), a band that holds no bin
    /// ([`Error::EmptyBand`]), no whole band between the edges
    /// ([`Error::EmptyFrequencyRange`]) or a top edge above half the sample
    /// rate ([`Error::AboveNyquist`]).
    #[must_use]
    pub fn banding(mut self, banding: Banding) -> Self {
        self.banding = banding;
        self
    }

    /// Smooths the power of each output from frame to frame.
    #[must_use]
    pub fn smoothing(mut self, smoothing: Smoothing) -> Self {
        self.smoothing = smoothing;
        self
    }

    /// The level silence and anything quieter reads.
    #[must_use]
    pub fn floor(mut self, floor: SpectrumFloor) -> Self {
        self.floor = floor;
        self
    }

    /// Holds each peak for `hold` before it falls.
    ///
    /// [`measure`](Self::measure) refuses a `hold` of more than 4294967295
    /// frames at the hop and rate in use ([`Error::PeakHoldTooLong`]).
    #[must_use]
    pub fn peak_hold(mut self, hold: SpectrumTime) -> Self {
        self.peak_hold = hold;
        self
    }

    /// Lets a held peak fall by `decay` per second once its hold is over.
    #[must_use]
    pub fn peak_decay(mut self, decay: PeakDecay) -> Self {
        self.peak_decay = decay;
        self
    }

    /// Keeps the newest `columns` frames of levels; 0 keeps none.
    ///
    /// [`measure`](Self::measure) refuses `columns` times the outputs of more
    /// than 4194304 cells ([`Error::SpectrogramTooLarge`]).
    #[must_use]
    pub fn spectrogram(mut self, columns: FrameCount) -> Self {
        self.columns = columns.get();
        self
    }

    /// A meter that analyses audio at `rate`, block by block. Builds every
    /// buffer and plans the transform, so that pushing allocates nothing.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroWindow`] or [`Error::ZeroHop`] for a zero window or hop;
    /// [`Error::WindowOutOfRange`] for a window under 64 or over 65536
    /// samples; [`Error::HopWiderThanWindow`] for a hop wider than the window;
    /// [`Error::TooManyOverlaps`] for a hop giving more than 64 frames per
    /// window; [`Error::SampleRateOutOfRange`] for a rate outside
    /// 1 Hz..=10 MHz; [`Error::TooManyBands`] for more than 4096 bands;
    /// [`Error::EmptyBand`] for a band that holds no bin;
    /// [`Error::EmptyFrequencyRange`] when no whole band fits between the
    /// edges; [`Error::AboveNyquist`] when the top edge exceeds half the sample
    /// rate; [`Error::SpectrogramTooLarge`] when the columns times the outputs
    /// exceed 4194304 cells; [`Error::PeakHoldTooLong`] when the peak hold is
    /// more than 4294967295 frames at the hop and rate in use.
    pub fn measure(&self, rate: impl Into<SampleRate>) -> Result<SpectrumMeter> {
        SpectrumMeter::build(&self.plan(rate.into())?)
    }

    fn plan(&self, rate: SampleRate) -> Result<SpectrumPlan> {
        let geometry = self.framing.geometry(rate)?;
        let window = geometry.window().get();
        if !(MIN_WINDOW..=MAX_WINDOW).contains(&window) {
            return Err(Error::WindowOutOfRange {
                window: Samples(window),
                min: Samples(MIN_WINDOW),
                max: Samples(MAX_WINDOW),
            });
        }
        let hop = geometry.hop().get();
        if hop > window {
            return Err(Error::HopWiderThanWindow {
                hop: geometry.hop(),
                window: Samples(window),
            });
        }
        if window.div_ceil(hop) > MAX_OVERLAP {
            return Err(Error::TooManyOverlaps {
                window: Samples(window),
                hop: geometry.hop(),
                max: MAX_OVERLAP,
            });
        }
        if !(MIN_RATE..=MAX_RATE).contains(&rate) {
            return Err(Error::SampleRateOutOfRange {
                rate,
                min: MIN_RATE,
                max: MAX_RATE,
            });
        }
        Ok(SpectrumPlan {
            geometry,
            banding: self.banding,
            smoothing: self.smoothing,
            floor: self.floor,
            peak_hold: self.peak_hold,
            peak_decay: self.peak_decay,
            columns: self.columns,
        })
    }
}

impl Default for SpectrumConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`SpectrumConfig`] at one sample rate, its framing checked.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SpectrumPlan {
    geometry: StftGeometry,
    banding: Banding,
    smoothing: Smoothing,
    floor: SpectrumFloor,
    peak_hold: SpectrumTime,
    peak_decay: PeakDecay,
    columns: usize,
}

impl SpectrumPlan {
    /// The first bin of each band and one past the last, `None` for per-bin
    /// outputs, once every setting that depends on the geometry is checked.
    fn layout(&self) -> Result<Option<Vec<usize>>> {
        let edges = self.band_edges()?;
        self.check_cells(edges.as_ref().map_or(self.bins(), |e| e.len() - 1))?;
        let frames = self.peak_hold_frames().get();
        if frames > u32::MAX as usize {
            return Err(Error::PeakHoldTooLong {
                frames,
                max: u32::MAX as usize,
            });
        }
        Ok(edges)
    }

    /// Time between frames.
    #[must_use]
    pub(crate) fn frame_interval(&self) -> Seconds {
        let hop = self.geometry.hop().get() as f64;
        Seconds(hop / self.geometry.sample_rate().get())
    }

    /// Whole frames a peak is held for: the hold rounded up to the hop.
    ///
    /// Worked in `f64` from the integer hop, since the `f32` frame interval
    /// would turn an exact multiple into one frame too many. The hold itself is
    /// an `f32` with a relative error of at most 2^-24 (6e-8), so a millionth
    /// of slack keeps 0.1 s at a hop of 480 samples and 48 kHz at 10 frames.
    #[must_use]
    pub(crate) fn peak_hold_frames(&self) -> FrameCount {
        let hop = self.geometry.hop().get() as f64;
        let frames = self.peak_hold.get().get() * self.geometry.sample_rate().get() / hop;
        FrameCount((frames * (1.0 - 1e-6)).ceil() as usize)
    }

    fn bins(&self) -> usize {
        self.geometry.window().get() / 2 + 1
    }

    fn bin_width(&self) -> f64 {
        self.geometry.sample_rate().get() / self.geometry.window().get() as f64
    }

    fn check_cells(&self, outputs: usize) -> Result<()> {
        let cells = self.columns.saturating_mul(outputs);
        if cells > MAX_SPECTROGRAM_CELLS {
            return Err(Error::SpectrogramTooLarge {
                cells,
                max: MAX_SPECTROGRAM_CELLS,
            });
        }
        Ok(())
    }

    /// The edge frequencies of the bands, `None` for per-bin outputs.
    fn edge_frequencies(&self) -> Result<Option<Vec<f64>>> {
        let (min, max, count, per_octave) = match self.banding {
            Banding::Bins => return Ok(None),
            Banding::Octave { per_octave, band } => {
                (band.min(), band.max(), 0, Some(per_octave.get()))
            }
            Banding::Log { count, band } => (band.min(), band.max(), count.get(), None),
        };
        let empty = Error::EmptyFrequencyRange { min, max };
        check_nyquist(max, self.geometry.sample_rate())?;
        let (lo, hi) = (f64::from(min.get()), f64::from(max.get()));
        let edges: Vec<f64> = match per_octave {
            Some(n) => {
                let bands = (n as f64 * libm::log2(hi / lo) + 1e-9).floor() as usize;
                if bands == 0 {
                    return Err(empty);
                }
                (0..=bands)
                    .map(|j| lo * libm::exp2(j as f64 / n as f64))
                    .collect()
            }
            None => (0..=count)
                .map(|j| lo * libm::pow(hi / lo, j as f64 / count as f64))
                .collect(),
        };
        Ok(Some(edges))
    }

    /// The first bin of each band and one past the last, `None` for per-bin
    /// outputs.
    fn band_edges(&self) -> Result<Option<Vec<usize>>> {
        let Some(edges) = self.edge_frequencies()? else {
            return Ok(None);
        };
        if edges.len() - 1 > MAX_BANDS {
            return Err(Error::TooManyBands {
                bands: edges.len() - 1,
                max: MAX_BANDS,
            });
        }
        let width = self.bin_width();
        let bins = self.bins();
        let first: Vec<usize> = edges
            .iter()
            .map(|&e| ((e / width).ceil() as usize).min(bins))
            .collect();
        if let Some(band) = first.windows(2).position(|w| w[0] >= w[1]) {
            // A band `w` Hz wide holds a bin once the bin spacing is within `w`.
            let narrowest = edges
                .windows(2)
                .map(|e| e[1] - e[0])
                .fold(f64::MAX, f64::min);
            let needed = (self.geometry.sample_rate().get() / narrowest).ceil();
            return Err(Error::EmptyBand {
                band,
                lower: Hz(edges[band] as f32),
                upper: Hz(edges[band + 1] as f32),
                window: Samples((needed.min((1u64 << 40) as f64) as usize).next_power_of_two()),
            });
        }
        Ok(Some(first))
    }
}

/// What a spectrum carries between blocks, and the readings.
///
/// A running spectrum at one rate: fed blocks of any length, it analyses a
/// frame whenever a hop of new samples has arrived, and holds the readings.
///
/// There is no `finish`: the result is the readings ([`levels`](Self::levels),
/// [`peaks`](Self::peaks), [`spectrogram`](Self::spectrogram)), which are
/// available after any push.
///
/// Pushing allocates nothing once the meter is built, but each frame is a
/// transform and every output is smoothed, so it belongs on the consumer side of
/// a tap, not in an audio callback.
pub struct SpectrumMeter {
    cfg: SpectrumPlan,
    fft: FftScratch,
    window: Vec<f32>,
    /// The first bin of each band and one past the last; empty for bins.
    band_bins: Vec<usize>,
    frequencies: Vec<Hz>,
    /// The last `window` samples; the next to write is at `write`.
    ring: Vec<f32>,
    write: usize,
    /// The ring unrolled oldest first, for the transform.
    frame: Vec<f32>,
    spectrum: Vec<Complex32>,
    fed: u64,
    next_due: u64,
    frames: u64,
    /// Smoothed linear power per output.
    power: Vec<f32>,
    levels: Vec<Db>,
    peaks: Vec<Db>,
    /// The level each peak last rose to, and the frames since.
    anchor: Vec<f32>,
    age: Vec<u32>,
    spectrogram: Vec<Db>,
    /// The next column to write, and how many hold a frame.
    head: usize,
    filled: usize,
}

impl core::fmt::Debug for SpectrumMeter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpectrumMeter")
            .field("cfg", &self.cfg)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl SpectrumMeter {
    /// The meter of a plan whose layout holds.
    fn build(cfg: &SpectrumPlan) -> Result<Self> {
        let mut fft = FftScratch::new();
        let edges = cfg.layout()?;
        let outputs = edges.as_ref().map_or(cfg.bins(), |e| e.len() - 1);
        let band_bins = edges.unwrap_or_default();
        let size = cfg.geometry.window().get();
        let width = cfg.bin_width();
        let frequencies: Vec<Hz> = match cfg.edge_frequencies()? {
            Some(edges) => edges
                .windows(2)
                .map(|e| Hz((e[0] * e[1]).sqrt() as f32))
                .collect(),
            None => (0..cfg.bins())
                .map(|k| Hz((k as f64 * width) as f32))
                .collect(),
        };
        let mut spectrum = vec![Complex32::default(); cfg.bins()];
        // Plans the transform and sizes its scratch.
        fft.forward(&vec![0.0; size], &vec![0.0; size], &mut spectrum);
        let mut state = Self {
            cfg: *cfg,
            fft,
            window: cfg.geometry.window_coefficients(),
            band_bins,
            frequencies,
            ring: vec![0.0; size],
            write: 0,
            frame: vec![0.0; size],
            spectrum,
            fed: 0,
            next_due: size as u64,
            frames: 0,
            power: vec![0.0; outputs],
            levels: vec![cfg.floor.get(); outputs],
            peaks: vec![cfg.floor.get(); outputs],
            anchor: vec![cfg.floor.get().get(); outputs],
            age: vec![0; outputs],
            spectrogram: vec![cfg.floor.get(); cfg.columns * outputs],
            head: 0,
            filled: 0,
        };
        state.reset();
        Ok(state)
    }

    /// Forgets everything fed, keeping the buffers: the readings are the
    /// floor and the next frame is ready after a whole window.
    pub fn reset(&mut self) {
        let floor = self.cfg.floor.get();
        self.ring.fill(0.0);
        self.write = 0;
        self.fed = 0;
        self.next_due = self.cfg.geometry.window().get() as u64;
        self.frames = 0;
        self.power.fill(0.0);
        self.levels.fill(floor);
        self.peaks.fill(floor);
        self.anchor.fill(floor.get());
        self.age.fill(0);
        self.spectrogram.fill(floor);
        self.head = 0;
        self.filled = 0;
    }

    /// The level of each output as of the last frame, [`floor`] before one.
    ///
    /// [`floor`]: SpectrumConfig::floor
    #[inline]
    #[must_use]
    pub fn levels(&self) -> &[Db] {
        &self.levels
    }

    /// The held peak of each output: never under its level.
    #[inline]
    #[must_use]
    pub fn peaks(&self) -> &[Db] {
        &self.peaks
    }

    /// The frequency of each output: a bin's, or the geometric centre of a
    /// band's edges.
    #[inline]
    #[must_use]
    pub fn frequencies(&self) -> &[Hz] {
        &self.frequencies
    }

    /// The kept columns of levels, oldest first, each as long as
    /// [`levels`](Self::levels). Empty without a spectrogram, and shorter than
    /// the configured columns until that many frames have been analysed.
    pub fn spectrogram(&self) -> impl ExactSizeIterator<Item = &[Db]> + '_ {
        let (columns, width) = (self.cfg.columns, self.levels.len());
        let start = (self.head + columns - self.filled) % columns.max(1);
        (0..self.filled).map(move |i| {
            let at = (start + i) % columns * width;
            &self.spectrogram[at..at + width]
        })
    }

    /// Analyses the ring as the frame that just became due.
    fn analyse(&mut self) {
        let cfg = self.cfg;
        // The ring's oldest sample is at `write`.
        let (newer, older) = self.ring.split_at(self.write);
        self.frame[..older.len()].copy_from_slice(older);
        self.frame[older.len()..].copy_from_slice(newer);
        self.fft
            .forward(&self.frame, &self.window, &mut self.spectrum);

        // `4 |X|^2 / (g N)^2`: a sine of amplitude `a` on a bin reads `a^2`. DC,
        // and Nyquist of an even window, have no image: `|X|^2 / (g N)^2`.
        let gain = f64::from(cfg.geometry.magnitude_gain());
        let scale = (4.0 / (gain * gain)) as f32;
        let size = self.ring.len();
        let power = |k: usize, bin: &Complex32| {
            let lone = k == 0 || (size.is_multiple_of(2) && k == size / 2);
            clamp_power(bin.norm_sqr() * if lone { scale / 4.0 } else { scale })
        };
        let smoothing = cfg.smoothing.ballistics();
        let dt = cfg.frame_interval();
        let first = self.frames == 0;
        // Validated to fit when the state was built.
        let hold = cfg.peak_hold_frames().get() as u32;
        // One product per call: exact, then rounded once to the `f32` the loop runs in.
        let decay = (f64::from(cfg.peak_decay.get().get()) * dt.get()) as f32;
        let floor = cfg.floor.get();

        for i in 0..self.power.len() {
            let raw = if self.band_bins.is_empty() {
                power(i, &self.spectrum[i])
            } else {
                let (lo, hi) = (self.band_bins[i], self.band_bins[i + 1]);
                let band = &self.spectrum[lo..hi];
                clamp_power(
                    band.iter()
                        .enumerate()
                        .map(|(j, b)| f64::from(power(lo + j, b)))
                        .sum::<f64>() as f32,
                )
            };
            let coefficient = smoothing.coeff_over(raw > self.power[i], dt);
            // A coefficient of 1 is no smoothing; assigning keeps the
            // reading exact where `old + (new - old)` would round against
            // a louder previous frame.
            let smoothed = if first || coefficient >= 1.0 {
                raw
            } else {
                Smoother::toward(self.power[i], raw, coefficient)
            };
            self.power[i] = if smoothed < POWER_EPSILON {
                0.0
            } else {
                smoothed
            };
            let level = to_db(self.power[i], floor);
            self.levels[i] = level;

            self.age[i] = self.age[i].saturating_add(1);
            let held = self.anchor[i] - decay * self.age[i].saturating_sub(hold) as f32;
            if level.get() >= held {
                self.anchor[i] = level.get();
                self.age[i] = 0;
                self.peaks[i] = level;
            } else {
                self.peaks[i] = Db(held.max(floor.get()));
            }
        }

        if cfg.columns > 0 {
            let width = self.levels.len();
            let at = self.head * width;
            self.spectrogram[at..at + width].copy_from_slice(&self.levels);
            self.head = (self.head + 1) % cfg.columns;
            self.filled = (self.filled + 1).min(cfg.columns);
        }
        self.frames += 1;
    }
}

/// A power kept finite and within `0..=MAX_POWER`; a NaN or infinity from
/// overflow in the transform reads as the maximum.
#[inline]
fn clamp_power(p: f32) -> f32 {
    if p.is_finite() {
        p.min(MAX_POWER)
    } else {
        MAX_POWER
    }
}

/// [`Db::from_power`], at least `floor`; zero power reads the floor.
#[inline]
fn to_db(power: f32, floor: Db) -> Db {
    if power > 0.0 {
        Db::from_power(power).max(floor)
    } else {
        floor
    }
}

impl SpectrumMeter {
    /// Feeds a block and analyses each frame that completes in it, in order.
    /// Returns whether at least one did; the readings are those of the last.
    ///
    /// A block may be any length, empty included, and may span several frames
    /// or none. Nothing is read from `block` ahead of the frame it belongs to,
    /// so the frames and readings are the same however the stream is cut.
    ///
    /// # Errors
    ///
    /// [`Error::NonFiniteInput`] for a NaN or infinite sample. The meter is
    /// untouched on error.
    pub fn push<S: Sample>(&mut self, block: Mono<'_, S>) -> Result<bool> {
        if !all_finite(block.samples()) {
            return Err(Error::NonFiniteInput);
        }
        let mut ready = false;
        for_blocks(block.samples(), 1, |converted| {
            ready |= self.step(converted);
        });
        Ok(ready)
    }

    /// Feeds converted samples and analyses each frame that completes in them.
    fn step(&mut self, block: &[f32]) -> bool {
        let size = self.ring.len();
        let mut rest = block;
        let mut ready = false;
        while !rest.is_empty() {
            // Up to the next due frame, and not past the end of the ring.
            let until_due = (self.next_due - self.fed).min(usize::MAX as u64) as usize;
            let take = rest.len().min(until_due).min(size - self.write);
            self.ring[self.write..self.write + take].copy_from_slice(&rest[..take]);
            self.write = (self.write + take) % size;
            self.fed += take as u64;
            rest = &rest[take..];
            if self.fed == self.next_due {
                self.analyse();
                self.next_due += self.cfg.geometry.hop().get() as u64;
                ready = true;
            }
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{RngExt, SeedableRng};
    use std::f64::consts::TAU;
    use tutti_types::SampleRate;
    use tutti_types::Samples;

    const RATE: f64 = 48_000.0;

    /// f32 transform and window error budget, in dB: a relative amplitude
    /// error of `1e-4` (about 100x `eps * log2(N)` for the sizes here) is
    /// `20 log10(1 + 1e-4)`.
    const FFT_TOL: f32 = 1e-3;

    fn band(min: f32, max: f32) -> FrequencyBand {
        FrequencyBand::new(Hz(min), Hz(max)).unwrap()
    }

    fn geometry(size: usize, hop: usize) -> StftGeometry {
        StftGeometry::new(SampleRate::of(RATE), Samples(size), Samples(hop)).unwrap()
    }

    fn config(size: usize, hop: usize) -> SpectrumConfig {
        SpectrumConfig::new().framing(Framing::new(Samples(size), Samples(hop)))
    }

    fn plan(cfg: &SpectrumConfig) -> SpectrumPlan {
        cfg.plan(SampleRate::of(RATE)).unwrap()
    }

    fn meter(cfg: &SpectrumConfig) -> SpectrumMeter {
        cfg.measure(SampleRate::of(RATE)).unwrap()
    }

    fn tone(len: usize, freq: f64, amp: f64) -> Vec<f32> {
        (0..len)
            .map(|i| (amp * (TAU * freq * i as f64 / RATE).sin()) as f32)
            .collect()
    }

    /// Feeds `signal` a hop at a time (so at most one frame completes per
    /// call), returning the state and the levels after each frame.
    fn run(cfg: &SpectrumConfig, signal: &[f32]) -> (SpectrumMeter, Vec<Vec<Db>>) {
        let mut state = meter(cfg);
        let mut frames = Vec::new();
        for block in signal.chunks(plan(cfg).geometry.hop().get()) {
            if state.push(Mono::new(block)).unwrap() {
                frames.push(state.levels().to_vec());
            }
        }
        (state, frames)
    }

    fn db(v: f64) -> f32 {
        v as f32
    }

    fn argmax(levels: &[Db]) -> usize {
        (0..levels.len())
            .max_by(|&a, &b| levels[a].get().total_cmp(&levels[b].get()))
            .unwrap()
    }

    /// A sine centred on a bin reads `20 log10(a)` there. The periodic Hann's
    /// coherent gain is 1/2, so `|X| = a N / 4` and `4 |X|^2 / (N/2)^2 = a^2`;
    /// the window puts half the amplitude (6.0206 dB down) on each neighbour
    /// and nothing beyond them.
    #[test]
    fn a_bin_centred_sine_reads_its_amplitude() {
        let size = 1536; // 1 kHz is bin 32 at 48 kHz
        let cfg = config(size, size / 4);
        let (state, frames) = run(&cfg, &tone(size * 3, 1000.0, 0.5));
        let want = db(20.0 * 0.5f64.log10());
        for levels in &frames {
            assert_eq!(argmax(levels), 32);
            assert!(
                (levels[32].get() - want).abs() < FFT_TOL,
                "{:?}",
                levels[32]
            );
            for k in [31, 33] {
                assert!(
                    (levels[k].get() - (want - 6.0206)).abs() < FFT_TOL,
                    "bin {k}: {:?}",
                    levels[k]
                );
            }
            // Leakage of a periodic Hann at a bin centre is exactly zero; what
            // is left is f32 rounding, far under -100 dB.
            assert!(levels[29].get() < -100.0 && levels[35].get() < -100.0);
        }
        assert_eq!(state.frequencies()[32], Hz(1000.0));
    }

    /// Off the centre the peak loses `|sinc(d) / (1 - d^2)|`, the Hann
    /// window's response at an offset of `d` bins, at most 1.4236 dB at half a
    /// bin. The tone's image at `-f` is 64 bins away, where the response is
    /// under -118 dB, so it adds under `1e-5` dB to the bound.
    #[test]
    fn an_off_centre_sine_loses_the_scalloping_figure() {
        let size = 1536;
        let cfg = config(size, size);
        let loss = |d: f64| {
            let sinc = (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d);
            20.0 * (sinc / (1.0 - d * d)).abs().log10()
        };
        assert!((loss(0.5) - -1.4236).abs() < 1e-3);
        for d in [0.1, 0.25, 0.4, 0.5] {
            let freq = (32.0 + d) * RATE / size as f64;
            let (_, frames) = run(&cfg, &tone(size, freq, 0.5));
            let levels = &frames[0];
            let peak = levels[argmax(levels)].get();
            let want = db(20.0 * 0.5f64.log10() + loss(d));
            assert!((peak - want).abs() < FFT_TOL, "d {d}: {peak} vs {want}");
            assert!(peak >= db(20.0 * 0.5f64.log10() - 1.4237) - FFT_TOL);
        }
    }

    /// Two tones resolve to two peaks at their bins and levels.
    ///
    /// A mis-unrolled ring is not caught here, and why: a tone of whole cycles per window is the same
    /// after a rotation of the ring, so a mis-unrolled ring leaves these levels
    /// alone; `frames_land_on_the_hop_grid_for_every_size_and_hop` catches it.
    #[test]
    fn two_tones_resolve_to_two_peaks() {
        let size = 1536;
        let cfg = config(size, size / 2);
        let (a, b) = (tone(size * 2, 1000.0, 0.5), tone(size * 2, 5000.0, 0.1));
        let mix: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
        let (_, frames) = run(&cfg, &mix);
        for levels in &frames {
            let peaks: Vec<usize> = (1..levels.len() - 1)
                .filter(|&k| {
                    levels[k] > levels[k - 1]
                        && levels[k] >= levels[k + 1]
                        && levels[k].get() > -60.0
                })
                .collect();
            assert_eq!(peaks, [32, 160]);
            assert!((levels[32].get() - db(20.0 * 0.5f64.log10())).abs() < FFT_TOL);
            assert!((levels[160].get() - db(20.0 * 0.1f64.log10())).abs() < FFT_TOL);
        }
    }

    /// Frames start at `k * hop` and end at `k * hop + size` for every size and
    /// hop, including a hop that does not divide the window, whatever the
    /// block: a unit impulse at `p` reads `w[p - k hop]` flat across the bins of
    /// each frame that holds it (`10 log10(4 w^2 / (N/2)^2)`), and the floor
    /// in the rest, and a frame is ready exactly at `k hop + size` samples.
    #[test]
    fn frames_land_on_the_hop_grid_for_every_size_and_hop() {
        for (size, hop) in [
            (64, 64),
            (64, 1),
            (100, 30),
            (256, 100),
            (257, 64),
            (1024, 256),
        ] {
            let cfg = config(size, hop);
            let impulse_at = size + 3 * hop + 5;
            let len = impulse_at + 2 * size;
            let mut signal = vec![0.0f32; len];
            signal[impulse_at] = 1.0;

            let mut state = meter(&cfg);
            let window = geometry(size, hop).window_coefficients();
            let gain = f64::from(geometry(size, hop).magnitude_gain());
            for (i, s) in signal.iter().enumerate() {
                let ready = state.push(Mono::new(&[*s])).unwrap();
                let fed = i + 1;
                let due = fed >= size && (fed - size) % hop == 0;
                assert_eq!(ready, due, "{size}/{hop} after {fed}");
                if !ready {
                    continue;
                }
                let start = fed - size;
                let level = match impulse_at.checked_sub(start).filter(|&o| o < size) {
                    Some(o) if window[o] > 0.0 => {
                        db(10.0 * (4.0 * f64::from(window[o]).powi(2) / (gain * gain)).log10())
                    }
                    _ => plan(&cfg).floor.get().get(),
                };
                for (k, l) in state.levels().iter().enumerate() {
                    // DC and an even window's Nyquist drop the factor 4.
                    let lone = k == 0 || (size.is_multiple_of(2) && k == size / 2);
                    let level = if lone && level > plan(&cfg).floor.get().get() {
                        level - 6.0206
                    } else {
                        level
                    };
                    assert!(
                        (l.get() - level).abs() < FFT_TOL,
                        "{size}/{hop} frame at {start}, bin {k}: {l:?} vs {level}"
                    );
                }
            }
        }
    }

    /// The same stream cut into blocks of 1, 7, a hop, a prime and the whole
    /// gives bit-identical levels, peaks, spectrogram and frame count.
    #[test]
    fn blocks_of_any_size_give_identical_frames() {
        let cfg = config(256, 100)
            .smoothing(Smoothing::Release(SpectrumTime::of(Seconds(0.05))))
            .banding(Banding::Octave {
                per_octave: BandsPerOctave::of(1),
                band: band(375.0, 12_000.0),
            })
            .spectrogram(FrameCount(6));
        let mut rng = StdRng::seed_from_u64(7);
        let signal: Vec<f32> = (0..9_000).map(|_| rng.random_range(-1.0..1.0)).collect();
        let read = |block: usize| {
            let mut state = meter(&cfg);
            for chunk in signal.chunks(block) {
                state.push(Mono::new(chunk)).unwrap();
            }
            let cols: Vec<Vec<Db>> = state.spectrogram().map(<[Db]>::to_vec).collect();
            (state.levels().to_vec(), state.peaks().to_vec(), cols)
        };
        let whole = read(signal.len());
        assert_eq!(whole.2.len(), 6);
        for block in [1, 7, 100, 1013, 4096] {
            assert_eq!(read(block), whole, "block {block}");
        }
    }

    /// White noise of variance `s^2` has `E|X_k|^2 = s^2 sum(w^2) = 3 N s^2 / 8`
    /// for Hann, so the level of the mean power is `10 log10(6 s^2 / N)`.
    /// Non-overlapping frames are independent, and bins 3 apart are uncorrelated
    /// (the transform of `w^2` has three terms), so each smoothed bin power is
    /// an exponential average of independent exponentials with relative
    /// variance `a / (2 - a)`, and the mean of `B` such bins has `a / ((2 - a) B)`.
    /// The tolerance is 5 sigma of that.
    ///
    /// The time series of that mean has a spread near `sqrt(a / (2 - a) / B)`
    /// against `1 / sqrt(B)` unsmoothed; asserting it under half the latter
    /// is what an averaging that does not average fails.
    ///
    /// Not covered: the first-frame seed, whose transient is under the
    /// tolerance after 600 frames.
    #[test]
    fn averaging_converges_to_the_mean_noise_power() {
        let size = 1024;
        let tau = Seconds(1.0);
        let cfg = config(size, size).smoothing(Smoothing::Average(SpectrumTime::of(tau)));
        let sigma = 0.1f64;
        let mut rng = StdRng::seed_from_u64(0x5eed);
        let mut gauss = || {
            let (u, v): (f64, f64) = (rng.random_range(1e-12..1.0), rng.random_range(0.0..1.0));
            sigma * (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
        };
        let frames = 600;
        let signal: Vec<f32> = (0..size * frames).map(|_| gauss() as f32).collect();

        let mut state = meter(&cfg);
        let picked: Vec<usize> = (3..size / 2 - 3).step_by(3).collect();
        let want = 6.0 * sigma * sigma / size as f64;
        let mut stats = Vec::new();
        for block in signal.chunks(size) {
            state.push(Mono::new(block)).unwrap();
            let mean = picked
                .iter()
                .map(|&k| 10f64.powf(f64::from(state.levels()[k].get()) / 10.0))
                .sum::<f64>()
                / picked.len() as f64;
            stats.push(mean / want);
        }
        let alpha = f64::from(
            Ballistics::new(tau, tau)
                .expect("a valid time")
                .coeff_over(true, plan(&cfg).frame_interval()),
        );
        let b = picked.len() as f64;
        let rel_sigma = (alpha / (2.0 - alpha) / b).sqrt();
        let last = *stats.last().unwrap();
        assert!(
            (last - 1.0).abs() < 5.0 * rel_sigma,
            "mean power ratio {last}, 5 sigma {}",
            5.0 * rel_sigma
        );

        let tail = &stats[stats.len() - 300..];
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        let spread =
            (tail.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        assert!(spread < 0.5 / b.sqrt(), "spread {spread}");
    }

    /// With instant attack and release time `tau`, power after the signal ends
    /// falls by `exp(-dt / tau)` per frame, so the level falls by
    /// `10 log10(e) dt / tau` dB per frame. A rise reads at once: a frame wholly
    /// inside the loud part reads the sine's level whatever came before.
    #[test]
    fn release_follows_the_analytic_exponential_and_attack_is_instant() {
        let (size, hop) = (1536, 512);
        let tau = Seconds(0.1);
        let cfg = config(size, hop).smoothing(Smoothing::Release(SpectrumTime::of(tau)));
        let (quiet, loud_end) = (size * 2, size * 6);
        let mut signal = tone(loud_end, 1000.0, 0.5);
        signal[..quiet].iter_mut().for_each(|s| *s *= 0.02);
        signal.resize(loud_end + size * 4, 0.0);
        let (_, frames) = run(&cfg, &signal);
        let at = |k: usize| frames[k][32].get();

        let want = db(20.0 * 0.5f64.log10());
        let first_loud = quiet.div_ceil(hop);
        for k in first_loud..=(loud_end - size) / hop {
            assert!((at(k) - want).abs() < FFT_TOL, "frame {k}: {}", at(k));
        }

        let step =
            10.0 * std::f64::consts::E.log10() * plan(&cfg).frame_interval().get() / tau.get();
        let first_silent = loud_end.div_ceil(hop);
        let mut checked = 0;
        for k in first_silent..frames.len() - 1 {
            let drop = f64::from(at(k) - at(k + 1));
            assert!((drop - step).abs() < 1e-3, "frame {k}: {drop} vs {step}");
            checked += 1;
        }
        assert!(checked >= 8);
    }

    /// A tone then silence: the peak holds its level for the hold rounded up
    /// to whole frames, then falls by the decay per second, `decay * dt` a
    /// frame.
    #[test]
    fn peaks_hold_then_decay_at_the_configured_rate() {
        let (size, hop) = (1536, 512);
        let (hold, decay) = (Seconds(0.25), Db(30.0));
        let cfg = config(size, hop)
            .peak_hold(SpectrumTime::of(hold))
            .peak_decay(PeakDecay::of(decay));
        let mut signal = tone(size * 3, 1000.0, 0.5);
        signal.resize(size * 3 + size * 40, 0.0);

        let mut state = meter(&cfg);
        let mut series = Vec::new();
        for block in signal.chunks(hop) {
            if state.push(Mono::new(block)).unwrap() {
                series.push((state.levels()[32].get(), state.peaks()[32].get()));
            }
        }
        // The last frame whose level is its own peak is where the hold starts.
        let anchor = series
            .iter()
            .rposition(|&(l, p)| l == p && l > -20.0)
            .unwrap();
        let dt = plan(&cfg).frame_interval().get();
        let held = plan(&cfg).peak_hold_frames().get();
        assert!(held as f64 * dt >= hold.get() && (held - 1) as f64 * dt < hold.get());
        assert_eq!(held, 24);

        let top = series[anchor].1;
        for j in 1..=held {
            assert_eq!(series[anchor + j].1, top, "held frame {j}");
        }
        for m in 1..30 {
            let want = top - decay.get() * dt as f32 * m as f32;
            let got = series[anchor + held + m].1;
            assert!(
                (got - want).abs() < 1e-3,
                "decay frame {m}: {got} vs {want}"
            );
        }
        // Never under the level.
        assert!(series.iter().all(|&(l, p)| p >= l));
    }

    /// The first `n` frames' columns are all there, and then the newest
    /// `columns` in order: a ramp of amplitudes makes every frame differ.
    #[test]
    fn the_spectrogram_keeps_the_newest_columns_in_order() {
        let (size, hop, columns) = (1536, 512, 5);
        let cfg = config(size, hop).spectrogram(FrameCount(columns));
        let ramp: Vec<f32> = (0..size + hop * 11)
            .map(|i| {
                let a = 0.01 + 0.04 * (i / hop) as f64;
                (a * (TAU * 1000.0 * i as f64 / RATE).sin()) as f32
            })
            .collect();

        let mut state = meter(&cfg);
        assert_eq!(state.spectrogram().len(), 0);
        let mut seen: Vec<Vec<Db>> = Vec::new();
        for block in ramp.chunks(hop) {
            if state.push(Mono::new(block)).unwrap() {
                seen.push(state.levels().to_vec());
                let cols: Vec<Vec<Db>> = state.spectrogram().map(<[Db]>::to_vec).collect();
                let n = seen.len().min(columns);
                assert_eq!(cols, seen[seen.len() - n..], "after {} frames", seen.len());
            }
        }
        assert!(seen.len() > columns * 2);
        assert!(seen.windows(2).all(|w| w[0][32] != w[1][32]));
    }

    /// An impulse at the middle of a Hann frame has `|X_k| = 1` in every bin,
    /// a flat spectrum, so a band reads `10 log10(n * 4 / (N/2)^2)` for the `n`
    /// bins it holds (counted here from the edges): power sums over a band, it
    /// is not averaged. All bands together hold every bin from `min` to the top
    /// edge.
    #[test]
    fn a_band_sums_the_power_of_its_bins() {
        let size = 4096;
        let width = RATE / size as f64;
        let bandings = [
            Banding::Octave {
                per_octave: BandsPerOctave::of(3),
                band: band(100.0, 20_000.0),
            },
            Banding::Log {
                count: BandCount::of(24),
                band: band(50.0, 20_000.0),
            },
            Banding::Octave {
                per_octave: BandsPerOctave::of(1),
                band: band(100.0, 12_800.0),
            },
        ];
        for banding in bandings {
            let cfg = config(size, size).banding(banding);
            let mut signal = vec![0.0f32; size];
            signal[size / 2] = 1.0;
            let (state, frames) = run(&cfg, &signal);

            let (min, bands, edge): (f64, usize, Box<dyn Fn(usize) -> f64>) = match banding {
                Banding::Octave { per_octave, band } => {
                    let (min, max) = (band.min(), band.max());
                    let n = (per_octave.get() as f64
                        * (f64::from(max.get()) / f64::from(min.get())).log2()
                        + 1e-9)
                        .floor() as usize;
                    let lo = f64::from(min.get());
                    (
                        lo,
                        n,
                        Box::new(move |j| lo * 2f64.powf(j as f64 / per_octave.get() as f64)),
                    )
                }
                Banding::Log { count, band } => {
                    let (min, max) = (band.min(), band.max());
                    let count = count.get();
                    let (lo, hi) = (f64::from(min.get()), f64::from(max.get()));
                    (
                        lo,
                        count,
                        Box::new(move |j| lo * (hi / lo).powf(j as f64 / count as f64)),
                    )
                }
                _ => unreachable!(),
            };
            assert_eq!(frames[0].len(), bands);
            assert_eq!(state.frequencies().len(), bands);
            let mut total = 0;
            for (j, (level, centre)) in frames[0].iter().zip(state.frequencies()).enumerate() {
                let n = (0..=size / 2)
                    .filter(|&k| {
                        let f = k as f64 * width;
                        f >= edge(j) && f < edge(j + 1)
                    })
                    .count();
                total += n;
                assert!(n >= 1);
                let want = db(10.0 * (n as f64 * 4.0 / (size as f64 / 2.0).powi(2)).log10());
                assert!(
                    (level.get() - want).abs() < FFT_TOL,
                    "{banding:?} band {j} ({n} bins): {level:?} vs {want}"
                );
                let geometric_mean = (edge(j) * edge(j + 1)).sqrt();
                assert!((f64::from(centre.get()) / geometric_mean - 1.0).abs() < 1e-6);
            }
            let held = (0..=size / 2)
                .filter(|&k| (k as f64 * width) >= min && (k as f64 * width) < edge(bands))
                .count();
            assert_eq!(total, held);
        }
    }

    /// A band with no bin is refused with the band, its edges and a window that
    /// holds every band. Third-octave bands from 20 Hz at 48 kHz: the second
    /// (25.2 to 31.7 Hz) sits between the bins of a 4096-point window, 11.7 Hz
    /// apart. The window named is for the narrowest band, the first, 5.198 Hz
    /// wide: `rate / width` = 9234 rounded up to a power of two, 16384, where
    /// the same banding is accepted.
    #[test]
    fn an_empty_band_names_itself_and_the_window_that_holds_it() {
        let banding = Banding::Octave {
            per_octave: BandsPerOctave::of(3),
            band: band(20.0, 20_000.0),
        };
        let Err(Error::EmptyBand {
            band,
            lower,
            upper,
            window,
        }) = config(4096, 1024)
            .banding(banding)
            .measure(SampleRate::of(RATE))
        else {
            panic!("a 4096-point window cannot hold the lowest third-octave band");
        };
        assert_eq!(band, 1);
        assert!((lower.get() - 20.0 * 2f32.powf(1.0 / 3.0)).abs() < 1e-4);
        assert!((upper.get() - 20.0 * 2f32.powf(2.0 / 3.0)).abs() < 1e-4);
        assert_eq!(window, Samples(16_384));
        assert!(
            config(16_384, 4096)
                .banding(banding)
                .measure(SampleRate::of(RATE))
                .is_ok()
        );
    }

    /// DC and an even window's Nyquist bin have no image, so a DC offset `A`
    /// reads `20 log10(A)` (the Hann window sums to `N/2`, `|X_0| = A N / 2`)
    /// and a Nyquist tone `A (-1)^n` does too.
    #[test]
    fn dc_and_nyquist_read_their_amplitude() {
        let size = 1024;
        let cfg = config(size, size);
        let want = db(20.0 * 0.25f64.log10());
        let (_, dc) = run(&cfg, &vec![0.25f32; size]);
        assert!((dc[0][0].get() - want).abs() < FFT_TOL, "{:?}", dc[0][0]);
        let nyquist: Vec<f32> = (0..size)
            .map(|i| if i % 2 == 0 { 0.25 } else { -0.25 })
            .collect();
        let (_, frames) = run(&cfg, &nyquist);
        assert!(
            (frames[0][size / 2].get() - want).abs() < FFT_TOL,
            "{:?}",
            frames[0][size / 2]
        );
    }

    /// A bin-centred sine inside one band reads its level plus `10 log10(1.5)`
    /// = 1.76 dB: Hann's main bin and two half-amplitude neighbours sum to
    /// `1 + 2 * (1/2)^2` = 1.5 of the sine's power.
    #[test]
    fn a_sine_inside_a_band_reads_the_noise_bandwidth_excess() {
        let size = 1536;
        let cfg = config(size, size).banding(Banding::Octave {
            per_octave: BandsPerOctave::of(1),
            band: band(700.0, 2800.0),
        });
        let (state, frames) = run(&cfg, &tone(size, 1000.0, 0.5));
        let want = db(20.0 * 0.5f64.log10() + 10.0 * 1.5f64.log10());
        let band = state
            .frequencies()
            .iter()
            .position(|f| f.get() < 1000.0 && f.get() > 900.0);
        let level = frames[0][band.unwrap()].get();
        assert!((level - want).abs() < FFT_TOL, "{level} vs {want}");
    }

    /// The hold is a whole number of frames worked from the integer hop, so an
    /// exact multiple is not over-held by the `f32` frame interval: 1.0 s at a
    /// hop of 480 samples and 48 kHz is 100 frames, not 101, and 0.1 s (an `f32`
    /// a hair over a tenth) is 10.
    #[test]
    fn the_hold_is_a_whole_number_of_frames_without_over_holding() {
        let cfg = config(1920, 480);
        let frames = |hold: f32| {
            plan(&cfg.peak_hold(SpectrumTime::new(Seconds::from_f32(hold)).unwrap()))
                .peak_hold_frames()
                .get()
        };
        assert_eq!(frames(1.0), 100);
        assert_eq!(frames(0.1), 10);
        assert_eq!(frames(0.0), 0);
        assert_eq!(frames(0.25), 25);
        // A hair over a frame holds the next.
        assert_eq!(frames(0.0101), 2);
        assert!(matches!(
            config(64, 1)
                .peak_hold(SpectrumTime::of(Seconds(3600.0)))
                .peak_decay(PeakDecay::of(Db(1.0)))
                .measure(SampleRate::of(10_000_000.0))
                .map(|_| ()),
            Err(Error::PeakHoldTooLong { .. })
        ));
    }

    /// The rules that span fields are checked, and sizes are capped before
    /// anything is allocated.
    #[test]
    fn bad_settings_are_errors() {
        let new = |size, hop| config(size, hop).measure(SampleRate::of(RATE));
        for size in [63, 65_537] {
            assert!(matches!(
                new(size, size),
                Err(Error::WindowOutOfRange { .. })
            ));
        }
        assert!(matches!(
            new(256, 257),
            Err(Error::HopWiderThanWindow { .. })
        ));
        for (size, hop) in [(65_536, 1), (4096, 63)] {
            assert!(matches!(new(size, hop), Err(Error::TooManyOverlaps { .. })));
        }
        assert!(new(64, 1).is_ok() && new(65_536, 65_536).is_ok() && new(4096, 64).is_ok());
        for rate in [0.5, 1e9] {
            assert!(matches!(
                config(256, 64).measure(SampleRate::of(rate)),
                Err(Error::SampleRateOutOfRange { .. })
            ));
        }

        let cfg = config(1024, 256);
        assert!(matches!(
            cfg.spectrogram(FrameCount(usize::MAX))
                .measure(SampleRate::of(RATE))
                .map(|_| ()),
            Err(Error::SpectrogramTooLarge { .. })
        ));
        assert_eq!(
            cfg.spectrogram(FrameCount((1 << 22) / 513 + 1))
                .measure(SampleRate::of(RATE))
                .map(|_| ()),
            Err(Error::SpectrogramTooLarge {
                cells: ((1 << 22) / 513 + 1) * 513,
                max: 1 << 22
            })
        );
        assert!(
            cfg.spectrogram(FrameCount((1 << 22) / 513))
                .measure(SampleRate::of(RATE))
                .map(|_| ())
                .is_ok()
        );
        let banded = cfg
            .banding(Banding::Log {
                count: BandCount::of(4000),
                band: band(100.0, 20_000.0),
            })
            .measure(SampleRate::of(RATE))
            .map(|_| ());
        assert!(
            matches!(banded, Err(Error::EmptyBand { .. })),
            "4000 bands of 513 bins cannot all hold a bin"
        );

        let oct = |per_octave: usize, min: f32, max: f32| {
            cfg.banding(Banding::Octave {
                per_octave: BandsPerOctave::new(per_octave).unwrap(),
                band: band(min, max),
            })
        };
        assert!(matches!(
            oct(48, 1e-30, 20_000.0)
                .measure(SampleRate::of(RATE))
                .map(|_| ()),
            Err(Error::TooManyBands { max: 4096, .. })
        ));
        assert!(
            matches!(
                oct(48, 100.0, 20_000.0)
                    .measure(SampleRate::of(RATE))
                    .map(|_| ()),
                Err(Error::EmptyBand { .. })
            ),
            "1/48 octave is finer than the bins"
        );
        for (min, max) in [
            (f32::NAN, 1000.0),
            (100.0, f32::INFINITY),
            (0.0, 1000.0),
            (1000.0, 100.0),
        ] {
            assert!(
                matches!(
                    FrequencyBand::try_from(Hz(min)..=Hz(max)),
                    Err(Error::EmptyFrequencyRange { .. })
                ),
                "{min} {max}"
            );
        }
        assert!(matches!(
            oct(1, 100.0, 30_000.0)
                .measure(SampleRate::of(RATE))
                .map(|_| ()),
            Err(Error::AboveNyquist { .. })
        ));
        assert!(matches!(
            oct(1, 100.0, 150.0)
                .measure(SampleRate::of(RATE))
                .map(|_| ()),
            Err(Error::EmptyFrequencyRange { .. })
        ));
        // Returning to per-bin outputs multiplies the cells per column: a
        // spectrogram sized for 17 bands is not one for 513 bins.
        let banded = oct(3, 400.0, 20_000.0).spectrogram(FrameCount(100_000));
        assert!(banded.measure(SampleRate::of(RATE)).map(|_| ()).is_ok());
        assert!(matches!(
            banded
                .banding(Banding::Bins)
                .measure(SampleRate::of(RATE))
                .map(|_| ()),
            Err(Error::SpectrogramTooLarge { .. })
        ));
    }

    /// The floor is where silence reads, and no level reads under it: a tone
    /// far below a -60 dB floor reads the floor in every bin.
    #[test]
    fn no_level_reads_under_the_configured_floor() {
        let cfg = config(256, 64).floor(SpectrumFloor::of(Db(-60.0)));
        let mut state = meter(&cfg);
        state.push(Mono::new(&vec![0.0f32; 1000])).unwrap();
        assert!(state.levels().iter().all(|&l| l == Db(-60.0)));
        state.push(Mono::new(&tone(1000, 1000.0, 1e-6))).unwrap();
        assert!(state.levels().iter().all(|&l| l == Db(-60.0)));
    }

    /// A refused block leaves the state as it was; silence reads the floor;
    /// a huge finite block stays finite; `reset` returns to the first state.
    #[test]
    fn input_is_checked_and_state_stays_finite() {
        let cfg = config(256, 64).spectrogram(FrameCount(4));
        let mut state = meter(&cfg);
        assert!(state.levels().iter().all(|&l| l == plan(&cfg).floor.get()));

        state.push(Mono::new(&vec![0.0f32; 1000])).unwrap();
        assert!(
            state.levels().iter().all(|&l| l == plan(&cfg).floor.get()),
            "silence"
        );
        assert!(state.peaks().iter().all(|&l| l == plan(&cfg).floor.get()));

        let before = state.levels().to_vec();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut block = vec![0.1f32; 500];
            block[499] = bad;
            assert_eq!(state.push(Mono::new(&block)), Err(Error::NonFiniteInput));
            assert_eq!(state.levels().to_vec(), before);
        }

        state.push(Mono::new(&vec![3.0e38f32; 600])).unwrap();
        assert!(
            state
                .levels()
                .iter()
                .chain(state.peaks())
                .all(|l| l.get().is_finite())
        );
        assert!(state.levels().iter().all(|l| l.get() <= 200.0));
        state.push(Mono::new(&vec![0.0f32; 600])).unwrap();
        assert!(state.levels().iter().all(|l| l.get().is_finite()));

        state.reset();
        assert!(
            state
                .levels()
                .iter()
                .chain(state.peaks())
                .all(|&l| l == plan(&cfg).floor.get())
        );
        assert_eq!(state.spectrogram().len(), 0);
    }
}
