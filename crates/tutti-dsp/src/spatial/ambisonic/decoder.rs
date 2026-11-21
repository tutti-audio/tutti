//! [`AmbisonicDecoder`]: an ambisonic signal to the feeds of a speaker array.

use tutti_buffers::{AmbisonicOrder, Float, MAX_CHANNELS, MonoMut, Planes};
use tutti_units::{Azimuth, Direction, Elevation, Gain, Spread};

use super::harmonics::{HarmonicNormalization, Harmonics, degree_of};
use crate::SpeakerLayout;

/// Latitudes of the virtual speakers of [`AmbisonicDecoder::allrad`]: the 4-point
/// Gauss-Legendre rule, exact for polynomials of degree 7 in the sine of the elevation.
const LATITUDES: usize = 4;

/// Bearings per latitude of the virtual speakers: 16 equal steps, exact for trigonometric
/// polynomials to degree 15 and a multiple of the 8-fold symmetry of a speaker ring.
const BEARINGS: usize = 16;

/// A decoder matrix: speaker `s` is `sum_c matrix[s][c] a_c` of the ambisonic channels
/// `a_c`.
///
/// Both decoders are built once, on the control side, from a [`SpeakerLayout`]; a
/// plane wave encoded by [`Harmonics`] decodes to gains that sum to one across the
/// speakers where the layout is regular enough (see [`sampling`](Self::sampling)), so a
/// source keeps its level wherever it sits.
#[derive(Clone, Debug, PartialEq)]
pub struct AmbisonicDecoder {
    order: AmbisonicOrder,
    /// One row per speaker, `(order + 1)^2` columns used.
    rows: Vec<[f64; MAX_CHANNELS]>,
}

impl AmbisonicDecoder {
    /// The sampling decoder: speaker `s` takes `w (2n + 1) Y(n, m)(direction s) / scale`
    /// of channel `(n, m)`, with `w = 1 / speakers` and `scale` the normalization's factor
    /// over SN3D in degree `n` ([`HarmonicNormalization::scale`]): the harmonics sampled at
    /// the speaker, weighted by the degree.
    ///
    /// For a layout whose speakers form a spherical `2 order`-design (the vertices of a
    /// cube are a 3-design, of an icosahedron a 5-design), a plane wave's gains sum to one
    /// and their squares to `(order + 1)^2 / speakers`, whatever its direction. A layout
    /// that is not regular decodes with a level and a width that depend on the direction:
    /// [`allrad`](Self::allrad) is the decoder for those.
    #[must_use]
    pub fn sampling(
        order: AmbisonicOrder,
        normalization: HarmonicNormalization,
        layout: &SpeakerLayout,
    ) -> Self {
        let weight = 1.0 / layout.speaker_count() as f64;
        let rows = layout
            .directions()
            .iter()
            .map(|&d| virtual_row(order, normalization, d, weight))
            .collect();
        Self { order, rows }
    }

    /// All-round ambisonic decoding (Zotter and Frank, AllRAD, 2012): the signal is
    /// decoded to a dense set of virtual speakers by the sampling rule, with the weights
    /// of a quadrature, and each virtual speaker is panned to the real ones by the
    /// [`SpeakerLayout`]'s VBAP gains.
    ///
    /// The virtual speakers are 4 Gauss-Legendre latitudes by 16 bearings, offset by half a
    /// step from the front so none sits on a bearing of a regular ring: the quadrature is
    /// exact through the products of two harmonics of order 3, so the virtual decode is
    /// exact and every irregularity of the array is the panner's alone. A layout without
    /// speakers below the listener is panned by VBAP's own retreat of a source to the
    /// nearest triplet.
    #[must_use]
    pub fn allrad(
        order: AmbisonicOrder,
        normalization: HarmonicNormalization,
        layout: &SpeakerLayout,
    ) -> Self {
        let mut rows = vec![[0.0; MAX_CHANNELS]; layout.speaker_count()];
        let mut gains = vec![Gain::SILENT; layout.speaker_count()];
        for (direction, weight) in quadrature() {
            layout.gains_into(
                direction.azimuth,
                direction.elevation,
                Spread::POINT,
                &mut gains,
            );
            let row = virtual_row(order, normalization, direction, weight);
            for (out, g) in rows.iter_mut().zip(&gains) {
                for (o, v) in out.iter_mut().zip(&row) {
                    *o += g.get() * v;
                }
            }
        }
        Self { order, rows }
    }

    /// The order.
    #[must_use]
    pub const fn order(&self) -> AmbisonicOrder {
        self.order
    }

    /// The number of speakers.
    #[must_use]
    pub fn speakers(&self) -> usize {
        self.rows.len()
    }

    /// Speaker `speaker`'s gain on ambisonic channel `channel` (ACN), or zero outside the
    /// matrix.
    #[must_use]
    pub fn gain(&self, speaker: usize, channel: usize) -> f64 {
        if channel >= usize::from(self.order.channels().count()) {
            return 0.0;
        }
        self.rows.get(speaker).map_or(0.0, |row| row[channel])
    }

    /// Renders speaker `speaker`'s feed of `input` (the channels of the order) into `out`,
    /// as many frames as the input has.
    pub fn process<F: Float>(&self, speaker: usize, input: Planes<'_, F>, mut out: MonoMut<'_, F>) {
        let out = out.samples_mut();
        out.fill(F::ZERO);
        for c in 0..usize::from(self.order.channels().count()) {
            let g = F::from_design(self.gain(speaker, c));
            for (o, &x) in out.iter_mut().zip(input.plane(c).samples()) {
                *o += g * x;
            }
        }
    }
}

/// The virtual speakers of [`AmbisonicDecoder::allrad`] and their weights, which sum to
/// one: the product rule of [`LATITUDES`] Gauss-Legendre latitudes and [`BEARINGS`] equal
/// bearings, offset by half a step from the front.
fn quadrature() -> Vec<(Direction, f64)> {
    let root = libm::sqrt(6.0 / 5.0);
    let inner = libm::sqrt(3.0 / 7.0 - 2.0 / 7.0 * root);
    let outer = libm::sqrt(3.0 / 7.0 + 2.0 / 7.0 * root);
    let heavy = (18.0 + libm::sqrt(30.0)) / 36.0;
    let light = (18.0 - libm::sqrt(30.0)) / 36.0;
    let latitudes: [(f64, f64); LATITUDES] = [
        (-outer, light),
        (-inner, heavy),
        (inner, heavy),
        (outer, light),
    ];
    let mut points = Vec::with_capacity(LATITUDES * BEARINGS);
    for (z, rule) in latitudes {
        let elevation = Elevation::new(libm::asin(z).to_degrees());
        // Gauss-Legendre weights sum to two over `z` in -1..=1, the sphere's measure.
        let weight = rule / 2.0 / BEARINGS as f64;
        for k in 0..BEARINGS {
            let azimuth = Azimuth::new((k as f64 + 0.5) * 360.0 / BEARINGS as f64).wrap();
            points.push((Direction::new(azimuth, elevation), weight));
        }
    }
    points
}

/// One decoded direction's row: `weight (2n + 1) Y(n, m)(direction) / scale` per channel,
/// where `Y` is SN3D and `scale` the normalization's factor over it.
fn virtual_row(
    order: AmbisonicOrder,
    normalization: HarmonicNormalization,
    direction: Direction,
    weight: f64,
) -> [f64; MAX_CHANNELS] {
    let sn3d = Harmonics::new(order, HarmonicNormalization::Sn3d, direction);
    let mut row = [0.0; MAX_CHANNELS];
    for (acn, (r, y)) in row.iter_mut().zip(sn3d.iter()).enumerate() {
        let n = degree_of(acn);
        *r = weight * f64::from(2 * n + 1) * y / normalization.scale(n);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The weights sum to one and the rule integrates every product of two harmonics to
    /// third order exactly: `sum w Y_a Y_b = [a = b] / (2n + 1)` for SN3D channels of
    /// degree `n`, the orthogonality of the harmonics over the sphere (mean over the
    /// sphere, `1 / (2n + 1)` for SN3D).
    #[test]
    fn the_quadrature_weights_sum_to_one_and_the_harmonics_are_orthogonal_over_it() {
        let points = quadrature();
        let total: f64 = points.iter().map(|p| p.1).sum();
        assert!((total - 1.0).abs() < 1e-14);
        let order = AmbisonicOrder::MAX;
        let values: Vec<Harmonics> = points
            .iter()
            .map(|p| Harmonics::new(order, HarmonicNormalization::Sn3d, p.0))
            .collect();
        for a in 0..16 {
            for b in 0..16 {
                let integral: f64 = points
                    .iter()
                    .zip(&values)
                    .map(|(p, y)| p.1 * y.get(a) * y.get(b))
                    .sum();
                let expected = if a == b {
                    1.0 / f64::from(2 * degree_of(a) + 1)
                } else {
                    0.0
                };
                assert!(
                    (integral - expected).abs() < 1e-13,
                    "({a}, {b}): {integral}"
                );
            }
        }
    }
}
