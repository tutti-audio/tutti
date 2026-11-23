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
//! ([`MeterError::EmptyBand`]); the bands cannot be finer than the bins. A band of
//! width `w` Hz is sure to hold a bin once the window is at least `rate / w`
//! samples, so the narrowest band, the lowest, sets the window a banding
//! needs: third-octave bands from 20 Hz (the first is 5.2 Hz wide) at 48 kHz
//! need 16384 samples. The error names the first empty band and a window that
//! holds every band.
//!
//! # Smoothing, peaks, spectrogram
//!
//! [`Smoothing`] works on the linear power of each output with
//! [`Ballistics`]'s one-pole step, so an averaged level is the dB of an
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

use core::marker::PhantomData;
use core::ops::RangeInclusive;

use tutti_buffers::{Mono, Sample};
use tutti_units::{Ballistics, Db, Hz, SampleRate, Samples, Seconds};

use crate::convert::{all_finite, for_blocks};
use crate::framing::{Framing, check_hop_within, check_nyquist, check_overlap, check_window};
use crate::meters::chroma::FrequencyBand;
use crate::{EmptyFrequencyRange, Measure, MeterError, StreamMeter};
use tutti_dsp::BinIndex;
use tutti_dsp::Complex64;
use tutti_dsp::FftScratch;
use tutti_dsp::FrameCount;
use tutti_dsp::StftGeometry;
use tutti_dsp::smooth_toward;
use tutti_dsp::{log_band_edges, octave_band_edges};
use tutti_units::ranged;

/// The smallest and largest window accepted.
const WINDOWS: RangeInclusive<Samples> = Samples::new(64)..=Samples::new(1 << 16);

/// The most outputs a banding may have.
const MAX_BANDS: usize = 4096;

/// The most cells (columns times outputs) a spectrogram may hold: 16 MiB of
/// `f64`.
const MAX_SPECTROGRAM_CELLS: usize = 1 << 22;

/// The sample rates accepted, in hertz.
const MIN_RATE: SampleRate = SampleRate::of(1.0);
const MAX_RATE: SampleRate = SampleRate::of(10_000_000.0);

/// Power clamp: +200 dB.
const MAX_POWER: f64 = 1e20;

/// Smoothed power under this (-300 dB re 1, far under any level's floor) is set
/// to zero, so a long release ends at silence instead of decaying through
/// subnormals.
const POWER_EPSILON: f64 = 1e-30;

ranged! {
    /// A time constant or a hold: within `0..=3600` seconds.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "Seconds", into = "Seconds")
    )]
    SpectrumTime(Seconds), SpectrumTimeError, "a spectrum time within 0..=3600 s",
    |raw| raw.get() >= 0.0 && raw.get() <= 3600.0
}

ranged! {
    /// The level silence and anything quieter reads: within `-300..=0` dB.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "Db", into = "Db")
    )]
    SpectrumFloor(Db), SpectrumFloorError, "a spectrum floor within -300..=0 dB",
    |raw| raw.get() >= -300.0 && raw.get() <= 0.0
}

ranged! {
    /// How fast a held peak falls, in [`Db`] per second: within `0..=10000`. No
    /// unit covers a rate of decibels.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "Db", into = "Db")
    )]
    PeakDecay(Db), PeakDecayError, "a peak decay within 0..=10000 dB per second",
    |raw| raw.get() >= 0.0 && raw.get() <= 10_000.0
}

ranged! {
    /// Bands per octave: within `1..=48`.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "usize", into = "usize")
    )]
    BandsPerOctave(usize), BandsPerOctaveError, "a band count per octave within 1..=48",
    |raw| raw >= 1 && raw <= 48
}

ranged! {
    /// The number of log-spaced bands: within `1..=4096`.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "usize", into = "usize")
    )]
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
            Self::None => (Seconds::new(0.0), Seconds::new(0.0)),
            Self::Release(tau) => (Seconds::new(0.0), tau.get()),
            Self::Average(tau) => (tau.get(), tau.get()),
        };
        Ballistics::new(attack, release).expect("a `SpectrumTime` is within 0..=3600 s")
    }
}

/// How the bins of a frame are grouped into outputs.
///
/// A banding's lowest band is its narrowest, and it must hold a bin: the
/// window must be at least `rate / width` samples for a first band of `width`
/// Hz, else [`SpectrumConfig::measure`] refuses with [`MeterError::EmptyBand`].
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
            framing: Framing::new(Samples::new(2048), Samples::new(512)),
            banding: Banding::Bins,
            smoothing: Smoothing::None,
            floor: SpectrumFloor::of(Db::FLOOR),
            peak_hold: SpectrumTime::of(Seconds::new(1.0)),
            peak_decay: PeakDecay::of(Db::new(20.0)),
            columns: 0,
        }
    }

    /// The window and hop frames are taken at.
    ///
    /// [`measure`](Self::measure) refuses a window under 64 or over 65536
    /// samples ([`FramingError::WindowOutOfRange`](crate::FramingError::WindowOutOfRange)), a hop wider than the window
    /// ([`FramingError::HopWiderThanWindow`](crate::FramingError::HopWiderThanWindow)) and a hop giving more than 64 frames
    /// per window ([`FramingError::TooManyOverlaps`](crate::FramingError::TooManyOverlaps)).
    #[must_use]
    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    /// Groups the bins into bands.
    ///
    /// [`measure`](Self::measure) refuses a banding the framing cannot hold:
    /// more than 4096 bands ([`MeterError::TooManyBands`]), a band that holds no bin
    /// ([`MeterError::EmptyBand`]), no whole band between the edges
    /// ([`MeterError::EmptyFrequencyRange`]) or a top edge above half the sample
    /// rate ([`AboveNyquist`](crate::AboveNyquist)).
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
    /// frames at the hop and rate in use ([`MeterError::PeakHoldTooLong`]).
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
    /// than 4194304 cells ([`MeterError::SpectrogramTooLarge`]).
    #[must_use]
    pub fn spectrogram(mut self, columns: FrameCount) -> Self {
        self.columns = columns.get();
        self
    }

    fn plan(&self, rate: SampleRate) -> Result<SpectrumPlan, MeterError> {
        let geometry = self.framing.geometry(rate)?;
        check_window(geometry.window(), &WINDOWS)?;
        check_hop_within(geometry.window(), geometry.hop())?;
        check_overlap(geometry.window(), geometry.hop())?;
        if !(MIN_RATE..=MAX_RATE).contains(&rate) {
            return Err(MeterError::SampleRateOutOfRange {
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

impl<S: Sample> Measure<S> for SpectrumConfig {
    type Context = SampleRate;
    type Meter = SpectrumMeter<S>;

    /// A meter that analyses audio at `rate`, block by block. Builds every
    /// buffer and plans the transform, so that pushing allocates nothing.
    ///
    /// # Errors
    ///
    /// [`FramingError::ZeroWindow`](crate::FramingError::ZeroWindow) or [`FramingError::ZeroHop`](crate::FramingError::ZeroHop) for a zero window or hop;
    /// [`FramingError::WindowOutOfRange`](crate::FramingError::WindowOutOfRange) for a window under 64 or over 65536
    /// samples; [`FramingError::HopWiderThanWindow`](crate::FramingError::HopWiderThanWindow) for a hop wider than the window;
    /// [`FramingError::TooManyOverlaps`](crate::FramingError::TooManyOverlaps) for a hop giving more than 64 frames per
    /// window; [`MeterError::SampleRateOutOfRange`] for a rate outside
    /// 1 Hz..=10 MHz; [`MeterError::TooManyBands`] for more than 4096 bands;
    /// [`MeterError::EmptyBand`] for a band that holds no bin;
    /// [`MeterError::EmptyFrequencyRange`] when no whole band fits between the
    /// edges; [`AboveNyquist`](crate::AboveNyquist) when the top edge exceeds half the sample
    /// rate; [`MeterError::SpectrogramTooLarge`] when the columns times the outputs
    /// exceed 4194304 cells; [`MeterError::PeakHoldTooLong`] when the peak hold is
    /// more than 4294967295 frames at the hop and rate in use.
    fn measure(&self, rate: SampleRate) -> Result<SpectrumMeter<S>, MeterError> {
        SpectrumMeter::build(&self.plan(rate)?)
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
    fn layout(&self) -> Result<Option<Vec<usize>>, MeterError> {
        let edges = self.band_edges()?;
        self.check_cells(edges.as_ref().map_or(self.bins(), |e| e.len() - 1))?;
        let frames = self.peak_hold_frames().get();
        if frames > u32::MAX as usize {
            return Err(MeterError::PeakHoldTooLong {
                frames,
                max: u32::MAX as usize,
            });
        }
        Ok(edges)
    }

    /// Whole frames a peak is held for: the hold rounded up to the hop.
    ///
    /// The quotient of an exact multiple can land a rounding above the integer
    /// and turn into one frame too many, so a millionth of slack keeps 0.1 s at a
    /// hop of 480 samples and 48 kHz at 10 frames.
    #[must_use]
    pub(crate) fn peak_hold_frames(&self) -> FrameCount {
        let frames = self.peak_hold.get().get() / self.geometry.frame_interval().get();
        FrameCount((frames * (1.0 - 1e-6)).ceil() as usize)
    }

    fn bins(&self) -> usize {
        self.geometry.window().get() / 2 + 1
    }

    fn check_cells(&self, outputs: usize) -> Result<(), MeterError> {
        let cells = self.columns.saturating_mul(outputs);
        if cells > MAX_SPECTROGRAM_CELLS {
            return Err(MeterError::SpectrogramTooLarge {
                cells,
                max: MAX_SPECTROGRAM_CELLS,
            });
        }
        Ok(())
    }

    /// The edge frequencies of the bands, `None` for per-bin outputs.
    fn edge_frequencies(&self) -> Result<Option<Vec<Hz>>, MeterError> {
        let (min, max, edges) = match self.banding {
            Banding::Bins => return Ok(None),
            Banding::Octave { per_octave, band } => (
                band.min(),
                band.max(),
                octave_band_edges(band.min(), band.max(), per_octave.get()),
            ),
            Banding::Log { count, band } => (
                band.min(),
                band.max(),
                log_band_edges(band.min(), band.max(), count.get()),
            ),
        };
        check_nyquist(max, self.geometry.sample_rate())?;
        if edges.is_empty() {
            return Err(EmptyFrequencyRange { min, max }.into());
        }
        Ok(Some(edges))
    }

    /// The first bin of each band and one past the last, `None` for per-bin
    /// outputs.
    fn band_edges(&self) -> Result<Option<Vec<usize>>, MeterError> {
        let Some(edges) = self.edge_frequencies()? else {
            return Ok(None);
        };
        if edges.len() - 1 > MAX_BANDS {
            return Err(MeterError::TooManyBands {
                bands: edges.len() - 1,
                max: MAX_BANDS,
            });
        }
        let bins = self.bins();
        let first: Vec<usize> = edges
            .iter()
            .map(|&e| (self.geometry.bin_of(e).ceil() as usize).min(bins))
            .collect();
        if let Some(band) = first.windows(2).position(|w| w[0] >= w[1]) {
            // A band `w` Hz wide holds a bin once the bin spacing is within `w`.
            let narrowest = edges
                .windows(2)
                .map(|e| e[1].get() - e[0].get())
                .fold(f64::MAX, f64::min);
            let needed = (self.geometry.sample_rate().get() / narrowest).ceil();
            return Err(MeterError::EmptyBand {
                band,
                lower: edges[band],
                upper: edges[band + 1],
                window: Samples::new(
                    (needed.min((1u64 << 40) as f64) as usize).next_power_of_two(),
                ),
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
/// A push yields one item per analysed frame, and the readings
/// ([`levels`](Self::levels), [`peaks`](Self::peaks),
/// [`spectrogram`](Self::spectrogram)) are available after any push;
/// [`finish`](StreamMeter::finish) returns the levels.
///
/// Pushing allocates nothing once the meter is built, but each frame is a
/// transform and every output is smoothed, so it belongs on the consumer side of
/// a tap, not in an audio callback.
pub struct SpectrumMeter<S: Sample> {
    cfg: SpectrumPlan,
    fft: FftScratch<f64>,
    window: Vec<f64>,
    /// The first bin of each band and one past the last; empty for bins.
    band_bins: Vec<usize>,
    frequencies: Vec<Hz>,
    /// The last `window` samples; the next to write is at `write`.
    ring: Vec<f64>,
    write: usize,
    /// The ring unrolled oldest first, for the transform.
    frame: Vec<f64>,
    spectrum: Vec<Complex64>,
    fed: u64,
    next_due: u64,
    frames: u64,
    /// Smoothed linear power per output.
    power: Vec<f64>,
    levels: Vec<Db>,
    peaks: Vec<Db>,
    /// The level each peak last rose to, and the frames since.
    anchor: Vec<Db>,
    age: Vec<u32>,
    spectrogram: Vec<Db>,
    /// The next column to write, and how many hold a frame.
    head: usize,
    filled: usize,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> core::fmt::Debug for SpectrumMeter<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpectrumMeter")
            .field("cfg", &self.cfg)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl<S: Sample> SpectrumMeter<S> {
    /// The meter of a plan whose layout holds.
    fn build(cfg: &SpectrumPlan) -> Result<Self, MeterError> {
        let mut fft = FftScratch::new();
        let edges = cfg.layout()?;
        let outputs = edges.as_ref().map_or(cfg.bins(), |e| e.len() - 1);
        let band_bins = edges.unwrap_or_default();
        let size = cfg.geometry.window().get();
        let frequencies: Vec<Hz> = match cfg.edge_frequencies()? {
            Some(edges) => edges
                .windows(2)
                .map(|e| Hz::new((e[0].get() * e[1].get()).sqrt()))
                .collect(),
            None => (0..cfg.bins())
                .map(|k| cfg.geometry.bin_frequency(BinIndex(k)))
                .collect(),
        };
        let mut spectrum = vec![Complex64::default(); cfg.bins()];
        // Plans the transform and sizes its scratch.
        fft.forward(Mono::new(&vec![0.0; size]), &vec![0.0; size], &mut spectrum);
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
            anchor: vec![cfg.floor.get(); outputs],
            age: vec![0; outputs],
            spectrogram: vec![cfg.floor.get(); cfg.columns * outputs],
            head: 0,
            filled: 0,
            sample: PhantomData,
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
        self.anchor.fill(floor);
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
            .forward(Mono::new(&self.frame), &self.window, &mut self.spectrum);

        // `4 |X|^2 / (g N)^2`: a sine of amplitude `a` on a bin reads `a^2`. DC,
        // and Nyquist of an even window, have no image: `|X|^2 / (g N)^2`.
        let gain = cfg.geometry.magnitude_gain::<f64>();
        let scale = 4.0 / (gain * gain);
        let size = self.ring.len();
        let power = |k: usize, bin: &Complex64| {
            let lone = k == 0 || (size.is_multiple_of(2) && k == size / 2);
            clamp_power(bin.norm_sqr() * if lone { scale / 4.0 } else { scale })
        };
        let smoothing = cfg.smoothing.ballistics();
        let dt = cfg.geometry.frame_interval();
        let first = self.frames == 0;
        // Validated to fit when the state was built.
        let hold = cfg.peak_hold_frames().get() as u32;
        // The decay of one frame: one product per call.
        let decay = cfg.peak_decay.get().get() * dt.get();
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
                        .map(|(j, b)| power(lo + j, b))
                        .sum::<f64>(),
                )
            };
            let coefficient = smoothing.coeff_over(raw > self.power[i], dt);
            // A coefficient of 1 is no smoothing; assigning keeps the
            // reading exact where `old + (new - old)` would round against
            // a louder previous frame.
            let smoothed = if first || coefficient >= 1.0 {
                raw
            } else {
                smooth_toward(self.power[i], raw, coefficient)
            };
            self.power[i] = if smoothed < POWER_EPSILON {
                0.0
            } else {
                smoothed
            };
            let level = to_db(self.power[i], floor);
            self.levels[i] = level;

            self.age[i] = self.age[i].saturating_add(1);
            let held =
                self.anchor[i] - Db::new(decay * f64::from(self.age[i].saturating_sub(hold)));
            if level >= held {
                self.anchor[i] = level;
                self.age[i] = 0;
                self.peaks[i] = level;
            } else {
                self.peaks[i] = held.max(floor);
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
fn clamp_power(p: f64) -> f64 {
    if p.is_finite() {
        p.min(MAX_POWER)
    } else {
        MAX_POWER
    }
}

/// [`Db::from_power`], at least `floor`; zero power reads the floor. The power is an `f64`
/// transform figure.
#[inline]
fn to_db(power: f64, floor: Db) -> Db {
    if power > 0.0 {
        Db::from_power(power).max(floor)
    } else {
        floor
    }
}

impl<S: Sample> SpectrumMeter<S> {
    /// Feeds samples and analyses each frame that completes in them, calling
    /// `each` after every one.
    fn step(&mut self, block: &[f64], mut each: impl FnMut()) {
        let size = self.ring.len();
        let mut rest = block;
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
                each();
            }
        }
    }
}

impl<S: Sample> StreamMeter for SpectrumMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = Vec<Db>;

    /// Feeds a block, analyses each frame that completes in it, in order, and
    /// extends `out` with one `()` per frame; the readings are those of the
    /// last.
    ///
    /// A block may be any length, empty included, and may span several frames
    /// or none. Nothing is read from `block` ahead of the frame it belongs to,
    /// so the frames and readings are the same however the stream is cut.
    ///
    /// # Errors
    ///
    /// [`MeterError::NonFiniteInput`] for a NaN or infinite sample. The meter is
    /// untouched on error.
    fn push(&mut self, block: Mono<'_, S>, out: &mut impl Extend<()>) -> Result<(), MeterError> {
        if !all_finite(block.samples()) {
            return Err(MeterError::NonFiniteInput);
        }
        for_blocks(block.samples(), 1, |converted| {
            self.step(converted, || out.extend([()]));
        });
        Ok(())
    }

    /// The level of each output as of the last frame: [`levels`](Self::levels)
    /// at the end of the stream.
    fn finish(self) -> Vec<Db> {
        self.levels
    }
}

#[cfg(test)]
mod tests;
