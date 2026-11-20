//! The mel scale and the triangular mel filterbank over the bins of a magnitude spectrum.
//!
//! # The scale
//!
//! [`MelScale::Htk`] is O'Shaughnessy's `m = 2595 log10(1 + f / 700)` (*Speech
//! Communication*, 1987), the convention of the HTK toolkit. [`MelScale::Slaney`] is the
//! scale of Slaney's *Auditory Toolbox* (1998): linear, `f / (200/3)`, below 1 kHz
//! (15 mel) and logarithmic above, with a factor of `6.4` every 27 mel. Both pass through
//! zero and both are strictly increasing, so each has an inverse.
//!
//! # The bank
//!
//! A [`MelFilterbank`] is `bands` triangular filters over the bins of one magnitude
//! spectrum (`DC..=Nyquist`, bin `k` at `k * bin_width`). The `bands + 2` edge
//! frequencies are the inverse mel of equally spaced mels from `low` to `high`; filter `i`
//! rises from edge `i` to edge `i + 1` (its centre) and falls to edge `i + 2`, so adjacent
//! filters cross at half height over the span they share and a [`Peak`](MelNorm::Peak)
//! bank sums to one across it. A band's weights are [`MelNorm`] scaled.
//!
//! A band that holds no bin's weight is refused: its log energy would be the floor
//! whatever the signal. A longer window (narrower bins), fewer bands or a narrower range
//! cure it.

use tutti_buffers::{Float, Sample};
use tutti_units::Hz;

use crate::{BinCount, BinIndex};

tutti_units::ranged! {
    /// How many bands a [`MelFilterbank`] has, `1..=512`.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    MelBandCount(u16), MelBandCountError, "a band count within 1..=512",
    |raw| raw >= 1 && raw <= 512
}

impl MelBandCount {
    /// The count as an index bound.
    #[inline]
    pub(crate) fn len(self) -> usize {
        usize::from(self.get())
    }
}

/// Which frequency-to-mel map a filterbank spaces its edges by.
///
/// A mel value is a plain `f64`: no unit covers a perceptual pitch coordinate, and one
/// would need a range or an algebra of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MelScale {
    /// `2595 log10(1 + f / 700)`, O'Shaughnessy (1987), as in the HTK toolkit.
    #[default]
    Htk,
    /// Slaney's *Auditory Toolbox* (1998): `f / (200/3)` below 1 kHz, then
    /// `15 + ln(f / 1000) / (ln(6.4) / 27)`.
    Slaney,
}

/// Slaney's linear region's slope in Hz per mel, `200 / 3`.
const SLANEY_HZ_PER_MEL: f64 = 200.0 / 3.0;
/// Where Slaney's scale turns logarithmic.
const SLANEY_KNEE: f64 = 1000.0;
/// Slaney's logarithmic step, `ln(6.4) / 27` nepers per mel.
const SLANEY_LOG_STEP: f64 = 0.068_751_777_420_949_12;

impl MelScale {
    /// The mel value of `frequency`; `0.0` at 0 Hz.
    #[must_use]
    pub fn mel(self, frequency: Hz) -> f64 {
        let f = frequency.get();
        match self {
            Self::Htk => 2595.0 * libm::log10(1.0 + f / 700.0),
            Self::Slaney if f < SLANEY_KNEE => f / SLANEY_HZ_PER_MEL,
            Self::Slaney => {
                SLANEY_KNEE / SLANEY_HZ_PER_MEL + libm::log(f / SLANEY_KNEE) / SLANEY_LOG_STEP
            }
        }
    }

    /// The frequency of `mel`, the inverse of [`mel`](Self::mel).
    #[must_use]
    pub fn frequency(self, mel: f64) -> Hz {
        Hz::new(match self {
            Self::Htk => 700.0 * (libm::pow(10.0, mel / 2595.0) - 1.0),
            Self::Slaney if mel < SLANEY_KNEE / SLANEY_HZ_PER_MEL => mel * SLANEY_HZ_PER_MEL,
            Self::Slaney => {
                SLANEY_KNEE * libm::exp(SLANEY_LOG_STEP * (mel - SLANEY_KNEE / SLANEY_HZ_PER_MEL))
            }
        })
    }
}

/// How a mel filter's triangle is scaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MelNorm {
    /// Unit height at the centre frequency: HTK's triangles.
    #[default]
    Peak,
    /// Unit area over frequency (`2 / (upper - lower)` high), Slaney's `norm="slaney"`:
    /// the weights times the bin width sum to one, to the sampling of the triangle.
    Area,
    /// Weights that sum to one over the bins: a flat spectrum reads its own level in every
    /// band, exactly.
    Sum,
}

/// What a filterbank refuses at construction.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MelError {
    /// The bin width is not a positive finite frequency.
    #[error("bin width {0} is not positive and finite")]
    BinWidth(Hz),
    /// `low` is negative or `high` is not above it.
    #[error("empty mel range: {low} to {high}")]
    EmptyRange {
        /// The lower bound, in [`Hz`].
        low: Hz,
        /// The upper bound, in [`Hz`].
        high: Hz,
    },
    /// `high` is above the spectrum's last bin.
    #[error("mel range top {high} is above Nyquist {nyquist}")]
    AboveNyquist {
        /// The upper bound, in [`Hz`].
        high: Hz,
        /// The frequency of the last bin, in [`Hz`].
        nyquist: Hz,
    },
    /// A band whose triangle holds no bin: its weights would all be zero.
    #[error("mel band {band} ({lower} to {upper}) holds no bin")]
    EmptyBand {
        /// The lowest such band.
        band: usize,
        /// Its lower edge, in [`Hz`].
        lower: Hz,
        /// Its upper edge, in [`Hz`].
        upper: Hz,
    },
}

/// What a [`MelFilterbank`] is made from: the bank's size, the spectrum it reads, and the
/// options. By default the [`Htk`](MelScale::Htk) scale, [`Peak`](MelNorm::Peak)
/// triangles and the whole spectrum, `0 Hz` to the last bin.
///
/// `MelFilterbank::try_from(config)` designs the bank.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MelConfig {
    bands: MelBandCount,
    bins: BinCount,
    bin_width: Hz,
    low: Hz,
    high: Hz,
    scale: MelScale,
    norm: MelNorm,
}

impl MelConfig {
    /// A bank of `bands` filters over a spectrum of `bins` bins `bin_width` apart
    /// (`sample_rate / window`).
    #[must_use]
    pub fn new(bands: MelBandCount, bins: BinCount, bin_width: Hz) -> Self {
        Self {
            bands,
            bins,
            bin_width,
            low: Hz::ZERO,
            high: Hz::new(bins.get().saturating_sub(1) as f64 * bin_width.get()),
            scale: MelScale::Htk,
            norm: MelNorm::Peak,
        }
    }

    /// The bank spans `low` to `high` instead of the whole spectrum.
    #[must_use]
    pub fn range(mut self, low: Hz, high: Hz) -> Self {
        self.low = low;
        self.high = high;
        self
    }

    /// The scale the edges are equally spaced in.
    #[must_use]
    pub fn scale(mut self, scale: MelScale) -> Self {
        self.scale = scale;
        self
    }

    /// How each triangle is scaled.
    #[must_use]
    pub fn norm(mut self, norm: MelNorm) -> Self {
        self.norm = norm;
        self
    }
}

/// Where one band's weights sit in the bank.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    first: usize,
    offset: usize,
    len: usize,
}

/// Triangular filters equally spaced in mel over the bins of a magnitude spectrum. See the
/// module documentation.
#[derive(Debug, Clone, PartialEq)]
pub struct MelFilterbank {
    bands: MelBandCount,
    bins: BinCount,
    edges: Vec<Hz>,
    spans: Vec<Span>,
    weights: Vec<f64>,
}

impl TryFrom<MelConfig> for MelFilterbank {
    type Error = MelError;

    fn try_from(config: MelConfig) -> Result<Self, MelError> {
        let MelConfig {
            bands,
            bins,
            bin_width,
            low,
            high,
            scale,
            norm,
        } = config;
        let width = bin_width.get();
        if !(width.is_finite() && width > 0.0) {
            return Err(MelError::BinWidth(bin_width));
        }
        let nyquist = Hz::new(bins.get().saturating_sub(1) as f64 * width);
        if !(low.get() >= 0.0 && high.get() > low.get()) {
            return Err(MelError::EmptyRange { low, high });
        }
        if high.get() > nyquist.get() {
            return Err(MelError::AboveNyquist { high, nyquist });
        }
        let points = bands.len() + 1;
        let (m_low, m_high) = (scale.mel(low), scale.mel(high));
        let mut edges: Vec<Hz> = (0..=points)
            .map(|j| scale.frequency(m_low + (m_high - m_low) * j as f64 / points as f64))
            .collect();
        // The ends are the stated range, not its round trip through the scale.
        edges[0] = low;
        edges[points] = high;

        let mut spans = Vec::with_capacity(bands.len());
        let mut weights = Vec::new();
        for band in 0..bands.len() {
            let (lower, centre, upper) = (
                edges[band].get(),
                edges[band + 1].get(),
                edges[band + 2].get(),
            );
            let first_bin = libm::floor(lower / width) as usize;
            let last_bin = (libm::ceil(upper / width) as usize).min(bins.get() - 1);
            let triangle: Vec<f64> = (first_bin..=last_bin)
                .map(|k| {
                    let f = k as f64 * width;
                    let w = if f <= centre {
                        (f - lower) / (centre - lower)
                    } else {
                        (upper - f) / (upper - centre)
                    };
                    w.max(0.0)
                })
                .collect();
            let Some(start) = triangle.iter().position(|&w| w > 0.0) else {
                return Err(MelError::EmptyBand {
                    band,
                    lower: edges[band],
                    upper: edges[band + 2],
                });
            };
            let end = triangle.iter().rposition(|&w| w > 0.0).unwrap_or(start);
            let kept = &triangle[start..=end];
            let scale_by = match norm {
                MelNorm::Peak => 1.0,
                MelNorm::Area => 2.0 / (upper - lower),
                MelNorm::Sum => 1.0 / kept.iter().sum::<f64>(),
            };
            spans.push(Span {
                first: first_bin + start,
                offset: weights.len(),
                len: kept.len(),
            });
            weights.extend(kept.iter().map(|w| w * scale_by));
        }
        Ok(Self {
            bands,
            bins,
            edges,
            spans,
            weights,
        })
    }
}

impl MelFilterbank {
    /// The number of filters.
    #[must_use]
    pub fn bands(&self) -> MelBandCount {
        self.bands
    }

    /// The spectrum width the bank reads, in bins.
    #[must_use]
    pub fn bins(&self) -> BinCount {
        self.bins
    }

    /// The `bands + 2` edge frequencies, ascending: filter `i` has lower edge `edges[i]`,
    /// centre `edges[i + 1]` and upper edge `edges[i + 2]`.
    #[must_use]
    pub fn edges(&self) -> &[Hz] {
        &self.edges
    }

    /// The first bin with weight in `band` and its weights, one per bin from there; `None`
    /// past the last band. Bins outside the run have weight zero.
    #[must_use]
    pub fn weights<F: Float>(
        &self,
        band: usize,
    ) -> Option<(BinIndex, impl ExactSizeIterator<Item = F> + '_)> {
        let s = self.spans.get(band)?;
        let run = &self.weights[s.offset..s.offset + s.len];
        Some((BinIndex(s.first), run.iter().map(|&w| w.convert_to::<F>())))
    }

    /// Writes each band's energy, the weighted sum of the squared magnitudes, into
    /// `energies`, one per band, accumulated in `f64` and narrowed once. Reads the bins
    /// `magnitudes` holds and the bands `energies` has room for; allocates nothing.
    pub fn process<I: Float, O: Float>(&self, magnitudes: &[I], energies: &mut [O]) {
        for (energy, span) in energies.iter_mut().zip(&self.spans) {
            let weights = &self.weights[span.offset..span.offset + span.len];
            let bins = magnitudes.get(span.first..).unwrap_or(&[]);
            let sum: f64 = weights
                .iter()
                .zip(bins)
                .map(|(&w, &m)| {
                    let m = m.convert_to::<f64>();
                    w * m * m
                })
                .sum();
            *energy = sum.convert_to::<O>();
        }
    }
}

#[cfg(test)]
mod tests;
