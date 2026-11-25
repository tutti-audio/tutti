//! [`Harmonics`]: the real spherical harmonics of one direction, and the encoder that
//! plays a mono source through them.

use tutti_buffers::{AmbisonicNormalization, AmbisonicOrder, Float, MAX_CHANNELS, Mono, PlanesMut};
use tutti_units::Direction;

use crate::design::along;
use crate::{COEFF_INTERVAL, Lerp, Ramp};

/// How the channels of an ambisonic signal are scaled: AmbiX's SN3D or the fully
/// normalised N3D.
///
/// Both store the channels in ACN order. N3D is SN3D scaled by `sqrt(2n + 1)` in every
/// channel of degree `n`, so one scaling serves a whole degree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HarmonicNormalization {
    /// Schmidt semi-normalised: AmbiX's scaling, in which a plane wave's degree-`n`
    /// channels have squared sum one.
    Sn3d,
    /// Fully normalised: `sqrt(2n + 1)` over [`Sn3d`](Self::Sn3d) in degree `n`.
    N3d,
}

impl HarmonicNormalization {
    /// The factor over SN3D in the channels of degree `degree`: one for SN3D,
    /// `sqrt(2 degree + 1)` for N3D.
    #[must_use]
    pub fn scale(self, degree: u8) -> f64 {
        match self {
            Self::Sn3d => 1.0,
            Self::N3d => libm::sqrt(f64::from(2 * degree + 1)),
        }
    }
}

impl From<HarmonicNormalization> for AmbisonicNormalization {
    fn from(normalization: HarmonicNormalization) -> Self {
        match normalization {
            HarmonicNormalization::Sn3d => Self::Sn3d,
            HarmonicNormalization::N3d => Self::N3d,
        }
    }
}

/// The degree of ACN channel `acn`: `floor(sqrt(acn))`.
pub(super) const fn degree_of(acn: usize) -> u8 {
    let mut n = 0;
    while (n + 1) * (n + 1) <= acn {
        n += 1;
    }
    n as u8
}

/// The real spherical harmonics of a direction to an order, in ACN order: the gains a
/// plane wave from that direction takes into each ambisonic channel.
///
/// Channel `acn = n^2 + n + m` is the harmonic of degree `n` and index `m` in `-n..=n`
/// (`m < 0` a sine in the bearing, `m >= 0` a cosine), without the Condon-Shortley
/// phase: AmbiX's definition. The first four channels are `1`, `y`, `z`, `x`, with `x`
/// ahead, `y` to the left and `z` up.
///
/// ```
/// use tutti_buffers::AmbisonicOrder;
/// use tutti_dsp::{Harmonics, HarmonicNormalization};
/// use tutti_units::{Azimuth, Direction, Elevation};
///
/// let left = Direction::new(Azimuth::new(90.0), Elevation::LEVEL);
/// let h = Harmonics::new(AmbisonicOrder::of(1), HarmonicNormalization::Sn3d, left);
/// assert!((h.get(1) - 1.0).abs() < 1e-12); // y
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Harmonics {
    order: AmbisonicOrder,
    values: [f64; MAX_CHANNELS],
}

impl Harmonics {
    /// The harmonics of `direction` up to `order`, scaled by `normalization`.
    #[must_use]
    pub fn new(
        order: AmbisonicOrder,
        normalization: HarmonicNormalization,
        direction: Direction,
    ) -> Self {
        let mut values = [0.0; MAX_CHANNELS];
        sn3d_into(order.get(), direction, &mut values);
        for (acn, v) in values.iter_mut().enumerate() {
            *v *= normalization.scale(degree_of(acn));
        }
        Self { order, values }
    }

    /// The order.
    #[must_use]
    pub const fn order(&self) -> AmbisonicOrder {
        self.order
    }

    /// The value of channel `acn`, or zero past the channels of the order.
    #[must_use]
    pub fn get(&self, acn: usize) -> f64 {
        if acn < usize::from(self.order.channels().count()) {
            self.values[acn]
        } else {
            0.0
        }
    }

    /// The channels' values in ACN order: `(order + 1)^2` of them.
    pub fn iter(&self) -> impl Iterator<Item = f64> + '_ {
        self.values
            .iter()
            .copied()
            .take(usize::from(self.order.channels().count()))
    }
}

/// Linear in every channel.
impl Lerp for Harmonics {
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = along::<f64>(k, n);
        let mut values = [0.0; MAX_CHANNELS];
        for (v, (&from, &to)) in values.iter_mut().zip(self.values.iter().zip(&to.values)) {
            *v = crate::lerp(from, to, t);
        }
        Self {
            order: self.order,
            values,
        }
    }
}

/// The SN3D harmonics of `direction` up to `order` into `out`, in ACN order.
///
/// `Y(n, m) = sqrt((2 - [m = 0]) (n - |m|)! / (n + |m|)!) P(n, |m|)(sin elevation)`
/// times `cos(|m| azimuth)` for `m >= 0` and `sin(|m| azimuth)` for `m < 0`, with `P`
/// the associated Legendre function without the Condon-Shortley phase, by the upward
/// recurrence in the degree.
fn sn3d_into(order: u8, direction: Direction, out: &mut [f64; MAX_CHANNELS]) {
    let azimuth = direction.azimuth.get().to_radians();
    let elevation = direction.elevation.get().to_radians();
    let (sin_el, cos_el) = (libm::sin(elevation), libm::cos(elevation));
    // `P(m, m) = (2m - 1)!! cos(el)^m`, kept across the `m` loop.
    let mut diagonal = 1.0;
    for m in 0..=order {
        if m > 0 {
            diagonal *= f64::from(2 * m - 1) * cos_el;
        }
        // `p_prev` is `P(n - 2, m)` and `p` is `P(n - 1, m)` on entering degree `n`.
        let (mut p_prev, mut p) = (0.0, diagonal);
        for n in m..=order {
            if n > m + 1 {
                let next = (sin_el * f64::from(2 * n - 1) * p - f64::from(n + m - 1) * p_prev)
                    / f64::from(n - m);
                p_prev = p;
                p = next;
            } else if n == m + 1 {
                p_prev = p;
                p = sin_el * f64::from(2 * m + 1) * diagonal;
            }
            let legendre = p;
            // `(n - m)! / (n + m)!` as a product, `n + m` at most 6.
            let mut ratio = 1.0;
            for j in (n - m + 1)..=(n + m) {
                ratio /= f64::from(j);
            }
            let norm = libm::sqrt(if m == 0 { 1.0 } else { 2.0 } * ratio) * legendre;
            let centre = usize::from(n) * usize::from(n) + usize::from(n);
            out[centre + usize::from(m)] = norm * libm::cos(f64::from(m) * azimuth);
            if m > 0 {
                out[centre - usize::from(m)] = norm * libm::sin(f64::from(m) * azimuth);
            }
        }
    }
}

/// The encoder of a mono source at a direction into the channels of an order: the
/// source times the [`Harmonics`] of its direction, which glide to a new direction over
/// [`COEFF_INTERVAL`] frames.
#[derive(Clone, Copy, Debug)]
pub struct AmbisonicEncoder {
    normalization: HarmonicNormalization,
    gains: Ramp<Harmonics>,
}

impl AmbisonicEncoder {
    /// An encoder of `order` and `normalization`, placed at `direction`.
    #[must_use]
    pub fn new(
        order: AmbisonicOrder,
        normalization: HarmonicNormalization,
        direction: Direction,
    ) -> Self {
        Self {
            normalization,
            gains: Ramp::new(Harmonics::new(order, normalization, direction))
                .steps(COEFF_INTERVAL as u32),
        }
    }

    /// Glides to `direction` over [`COEFF_INTERVAL`] frames, from where the glide is now.
    pub fn retarget(&mut self, direction: Direction) {
        let order = self.gains.target().order();
        self.gains
            .retarget(Harmonics::new(order, self.normalization, direction));
    }

    /// Lands on the direction it glides to at once.
    pub fn jump_to_target(&mut self) {
        self.gains.jump_to_target();
    }

    /// Plays `input` into `output`, its first `(order + 1)^2` channels: channel `c` is the
    /// frame times the gain of harmonic `c`, blended along the glide.
    ///
    /// `output` has as many frames as `input`. A glide is read in closed form from the
    /// frames it has played, so a stretch renders the same bits however it is cut into
    /// calls.
    pub fn process<F: Float>(&mut self, input: Mono<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let input = input.samples();
        let frames = input.len();
        let blended = self.gains.blended_frames(frames);
        let first = self.gains.steps_taken() + 1;
        let (from, to) = (self.gains.from, self.gains.to);
        let mut t = [F::ZERO; COEFF_INTERVAL];
        for (j, t) in t.iter_mut().enumerate().take(blended) {
            *t = along::<F>(first + j as u32, self.gains.n);
        }
        for (c, (g0, g1)) in from.iter().zip(to.iter()).enumerate() {
            let mut out = output.plane_mut(c);
            let out = out.samples_mut();
            let (g0, g1) = (F::from_design(g0), F::from_design(g1));
            let (head, tail) = input.split_at(blended);
            let (out_head, out_tail) = out.split_at_mut(blended);
            for ((o, &x), &t) in out_head.iter_mut().zip(head).zip(&t) {
                *o = x * crate::lerp(g0, g1, t);
            }
            for (o, &x) in out_tail.iter_mut().zip(tail) {
                *o = x * g1;
            }
        }
        self.gains
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}
