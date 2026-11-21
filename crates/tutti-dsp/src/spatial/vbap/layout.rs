//! [`SpeakerLayout`]: the tuples of a speaker array and the gain solve.

use tutti_buffers::{Bed, MAX_CHANNELS};
use tutti_units::{ArcDegrees, Azimuth, Direction, Elevation, Gain, Radians, Spread};

use super::error::SpeakerGeometryError;
use super::math::{Mat2, Mat3, direction, inverse2, inverse3, mul2, mul3};
use super::select;

/// How many times [`SpeakerLayout::gains_into`] may retreat a source's elevation
/// toward the horizontal plane before giving up.
///
/// The retreat halves the distance to the plane each round, so the bound is a
/// number of halvings and not an angle. Measured over a 5 by 5 degree grid of
/// the beds, the worst case is four rounds (7.1.4, far below the array); the
/// horizontal layouts need at most one, and only exactly at the poles. Eight is therefore slack rather than a budget: it exists so the loop
/// is bounded on the audio thread, not because any real bearing approaches it.
const MAX_ELEVATION_RETREATS: u32 = 8;

/// Below this, the sum of squares of a gain vector (one tuple's candidates, or the solved
/// set) is treated as zero and renormalising it would divide by about zero.
const POWER_FLOOR: f64 = 1e-10;

/// A speaker above or below the horizontal plane by more than this makes a
/// layout [`PanningMode::Spherical`].
const PLANE_TOLERANCE: ArcDegrees = ArcDegrees::new(1e-6);

/// An elevation retreat that lands within this of the horizontal plane snaps
/// to it.
const ELEVATION_SNAP: ArcDegrees = ArcDegrees::new(1e-4);

/// Whether a layout pans between pairs of speakers or triplets.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
enum PanningMode {
    /// Every speaker is on the horizontal plane: a source lies between two.
    Horizontal,
    /// At least one speaker has height, so the speakers lie on a sphere, not
    /// a ring: a source lies inside a triangle of three.
    Spherical,
}

/// A pair or triplet of speakers a source can be placed between, with the
/// inverse of its direction matrix.
#[derive(Debug, Clone, Copy)]
struct Tuple {
    speakers: [u16; 3],
    inverse: Inverse,
}

#[derive(Debug, Clone, Copy)]
enum Inverse {
    Pair(Mat2),
    Triplet(Mat3),
}

impl Tuple {
    /// The speakers' indices: two for a horizontal layout, three otherwise.
    #[must_use]
    pub fn speakers(&self) -> &[u16] {
        match self.inverse {
            Inverse::Pair(_) => &self.speakers[..2],
            Inverse::Triplet(_) => &self.speakers[..3],
        }
    }
}

/// A speaker array, ready to pan over.
///
/// Built once on the control side from a [`Bed`] or a slice of [`Direction`]s (`TryFrom`); the solve
/// ([`gains_into`](Self::gains_into)) takes `&self` and allocates nothing, so one layout
/// serves any number of sources.
#[derive(Debug, Clone)]
pub struct SpeakerLayout {
    count: usize,
    /// The speakers' positions, in gain order.
    directions: Box<[Direction]>,
    tuples: Box<[Tuple]>,
    has_rear_speakers: bool,
}

impl TryFrom<Bed> for SpeakerLayout {
    type Error = SpeakerGeometryError;

    /// The layout of a [`Bed`], from its BS.2051 positions ([`Bed::direction`]).
    ///
    /// Gain `i` is the `i`-th placed speaker of the bed in the bed's own channel
    /// order ([`Bed::topology`]): the LFE has no direction and is never fed, so a
    /// bed with one has a gain slice one shorter than its width.
    ///
    /// # Errors
    ///
    /// [`SpeakerGeometryError`] when the bed's speakers cannot be panned over:
    /// [`Bed::Mono`] has one.
    fn try_from(bed: Bed) -> Result<Self, SpeakerGeometryError> {
        let directions: Vec<Direction> = bed
            .topology()
            .positions()
            .unwrap_or_default()
            .iter()
            .filter_map(|&speaker| bed.direction(speaker))
            .collect();
        Self::from_positions(&directions)
    }
}

impl TryFrom<&[Direction]> for SpeakerLayout {
    type Error = SpeakerGeometryError;

    /// A layout of speakers at the given bearings and heights, one gain per position.
    ///
    /// The refusal depends on the positions together (a speaker at the same place as another,
    /// or all in one plane through the listener), which no type of one position can hold.
    ///
    /// # Errors
    ///
    /// [`SpeakerGeometryError`] for too few or too many speakers, a position that is not
    /// finite, or positions no pair or triplet can be formed from.
    fn try_from(positions: &[Direction]) -> Result<Self, SpeakerGeometryError> {
        Self::from_positions(positions)
    }
}

impl SpeakerLayout {
    /// The layout of speakers at the given bearings and heights.
    ///
    /// The layout pans between pairs of speakers when every speaker is within a
    /// millionth of a degree of the horizontal plane, and between triplets
    /// otherwise. A speaker's index in `positions` is its index in every gain slice.
    ///
    /// # Errors
    ///
    /// [`SpeakerGeometryError`] for too few or too many speakers, a position
    /// that is not finite, or positions no pair or triplet can be formed from
    /// (a speaker at the same place as another, or all in one plane through
    /// the listener).
    fn from_positions(positions: &[Direction]) -> Result<Self, SpeakerGeometryError> {
        let count = positions.len();
        if count > MAX_CHANNELS {
            return Err(SpeakerGeometryError::TooManySpeakers {
                provided: count,
                max: MAX_CHANNELS,
            });
        }
        if positions
            .iter()
            .any(|d| !d.azimuth.get().is_finite() || !d.elevation.get().is_finite())
        {
            return Err(SpeakerGeometryError::NonFinitePosition);
        }
        let mode = if positions
            .iter()
            .any(|d| d.elevation.get().abs() > PLANE_TOLERANCE.get())
        {
            PanningMode::Spherical
        } else {
            PanningMode::Horizontal
        };
        let required = match mode {
            PanningMode::Horizontal => 2,
            PanningMode::Spherical => 3,
        };
        if count < required {
            return Err(SpeakerGeometryError::InsufficientSpeakers {
                provided: count,
                required,
            });
        }

        let tuples: Box<[Tuple]> = match mode {
            PanningMode::Horizontal => {
                let azimuths: Vec<Azimuth> = positions.iter().map(|d| d.azimuth).collect();
                select::pairs(&azimuths)
                    .into_iter()
                    .filter_map(|[a, b]| {
                        let azi_a = Radians::from(azimuths[usize::from(a)]);
                        let azi_b = Radians::from(azimuths[usize::from(b)]);
                        // The matrix's columns are the speakers' (sin, cos).
                        let (sin_a, cos_a) = (azi_a.sin(), azi_a.cos());
                        let (sin_b, cos_b) = (azi_b.sin(), azi_b.cos());
                        Some(Tuple {
                            speakers: [a, b, 0],
                            inverse: Inverse::Pair(inverse2([sin_a, cos_a], [sin_b, cos_b])?),
                        })
                    })
                    .collect()
            }
            PanningMode::Spherical => select::triplets(positions)
                .into_iter()
                .filter_map(|[i, j, k]| {
                    let column = |s: u16| direction(positions[usize::from(s)]);
                    Some(Tuple {
                        speakers: [i, j, k],
                        inverse: Inverse::Triplet(inverse3([column(i), column(j), column(k)])?),
                    })
                })
                .collect(),
        };
        if tuples.is_empty() {
            return Err(SpeakerGeometryError::NoValidTuples);
        }

        let has_rear_speakers = positions
            .iter()
            .any(|d| d.azimuth.wrap().get().abs() > 90.0);
        Ok(Self {
            count,
            directions: positions.into(),
            tuples,
            has_rear_speakers,
        })
    }

    /// The speakers' positions, in the order of every gain slice.
    #[must_use]
    pub fn directions(&self) -> &[Direction] {
        &self.directions
    }

    /// The number of speakers.
    #[must_use]
    pub(crate) fn speaker_count(&self) -> usize {
        self.count
    }

    /// Write the gains of one source at `azimuth` and `elevation`, with
    /// `spread` applied, into `out`: unit power (the sum of squares is 1) for
    /// every bearing and height, whatever the layout covers.
    ///
    /// `out` is the speaker count long, and all of it is
    /// written. A slice of another length is a caller's bug: a debug assertion
    /// fires, and a release build writes the first `min(out.len(),
    /// speaker_count)` gains (spread and unit power then hold over those) and
    /// leaves the rest of `out` as it was.
    ///
    /// # The three steps, and why each is load-bearing
    ///
    /// Pulkki's solve normalises a tuple's solution and then clamps its
    /// negative gains to zero. For a source inside the winning tuple no gain is
    /// negative and the order does not matter. For a source outside every
    /// tuple the clamp fires after the energy has been budgeted across gains
    /// that are then discarded, so the source quietly loses that energy, and
    /// where every gain is negative it is discarded entirely and the source is
    /// silent. Measured without the correction: a stereo pair held unity to 30
    /// degrees, then decayed (0.933 at 45, 0.500 at 90, 0.067 at 135) and was
    /// completely silent from 150 through 210.
    ///
    /// 1. Fold, for a front-only array. A stereo pair has no front/back axis
    ///    to resolve: both speakers are ahead, so no amplitude difference
    ///    could place a source behind. The nearest honest rendering of "behind
    ///    me and to the left" on such an array is "to the left", the mirror
    ///    about the lateral axis (135 becomes 45, 180 becomes 0). For an array
    ///    that does surround the listener the mirror would be a bug: a source at
    ///    135 has a real rear-left speaker to come out of. Folding alone is not
    ///    enough: it leaves 90 at half energy, because 90 is already in the
    ///    front hemisphere and outside the +-30 pair regardless.
    /// 2. Retreat in elevation, when the array has no geometry at all for the
    ///    requested height. The clamp has by then reduced the gain vector to
    ///    zeros, and the sign information it destroyed cannot be recovered.
    ///    Halving the elevation toward the horizontal plane and solving again
    ///    finds the nearest height the array can render: below an Atmos bed (no
    ///    speaker sits under the listener) and at the exact poles of a
    ///    horizontal ring (where the direction's horizontal projection is the
    ///    zero vector).
    /// 3. Renormalise: clamp-then-normalise completed, rescaling what survived
    ///    to unit energy. Renormalising alone is not enough either: it leaves
    ///    150 to 210 silent on stereo, where both gains were negative.
    ///
    /// The audible consequence is that a source outside the array's coverage
    /// hard-pans at full energy to the nearest speakers instead of fading
    /// out: a front-only pair genuinely cannot place a rear image, and
    /// collapsing to the nearest speaker keeps an automation audible all the
    /// way round.
    ///
    /// A direction that is not finite (a NaN bearing or height) gives silence:
    /// every gain zero, spread included.
    pub fn gains_into(
        &self,
        azimuth: Azimuth,
        elevation: Elevation,
        spread: Spread,
        out: &mut [Gain],
    ) {
        debug_assert_eq!(out.len(), self.count);
        let mut scratch = [0.0f64; MAX_CHANNELS];
        self.unit_power_gains(azimuth, elevation, &mut scratch[..self.count]);
        let written = out.len().min(self.count);
        for (out, &g) in out.iter_mut().zip(&scratch[..written]) {
            *out = Gain::new(g);
        }
        // Silence stays silent: spread would blend in an equal gain.
        if scratch[..written].iter().all(|&g| g == 0.0) {
            return;
        }
        apply_spread(spread, &mut out[..written]);
    }

    /// Steps 1 to 3 of [`gains_into`](Self::gains_into), in `f64`, into `scratch`
    /// (`speaker_count` long).
    fn unit_power_gains(&self, azimuth: Azimuth, elevation: Elevation, scratch: &mut [f64]) {
        let azimuth = if self.has_rear_speakers {
            azimuth.wrap()
        } else {
            fold_to_front(azimuth)
        };

        let mut elevation = Elevation::new_clamped(elevation.get());

        for _ in 0..MAX_ELEVATION_RETREATS {
            self.tuple_gains(Direction::new(azimuth, elevation), scratch);

            let sum_sq: f64 = scratch.iter().map(|g| g * g).sum();
            if sum_sq > POWER_FLOOR {
                let norm = 1.0 / sum_sq.sqrt();
                for gain in scratch.iter_mut() {
                    *gain *= norm;
                }
                return;
            }

            // Nothing survived the clamp at this height. Halve the distance to
            // the horizontal plane, which every supported layout covers, and
            // try again. Snapping the last sliver to exactly 0 rather than
            // halving forever is what keeps the bound at a few rounds in practice.
            elevation = elevation.lerp(Elevation::LEVEL, 0.5);
            if elevation.get().abs() < ELEVATION_SNAP.get() {
                elevation = Elevation::LEVEL;
            }
        }

        // No energy after the retreats: only a direction that is not a number
        // reaches here (a finite one finds the horizontal plane, which every
        // bed covers). The answer is silence, never a panic on the audio thread.
        scratch.fill(0.0);
    }

    /// Pulkki's solve: the gains of the tuple whose smallest candidate gain is
    /// largest (the tuple the direction lies inside), scaled to unit power
    /// and then clamped at zero. `gains` is `speaker_count` long; all of it is
    /// written.
    pub(super) fn tuple_gains(&self, aim: Direction, gains: &mut [f64]) {
        debug_assert_eq!(gains.len(), self.count);
        gains.fill(0.0);
        let p = direction(aim);

        let mut best: Option<(usize, [f64; 3], usize)> = None;
        let mut best_min = f64::NEG_INFINITY;
        for (at, tuple) in self.tuples.iter().enumerate() {
            let (candidate, len) = match tuple.inverse {
                Inverse::Triplet(m) => (mul3(m, p), 3),
                Inverse::Pair(m) => {
                    let g = mul2(m, [p[0], p[1]]);
                    ([g[0], g[1], 0.0], 2)
                }
            };
            let smallest = candidate[..len]
                .iter()
                .copied()
                .reduce(f64::min)
                .unwrap_or(f64::NEG_INFINITY);
            if smallest > best_min {
                best_min = smallest;
                best = Some((at, candidate, len));
            }
        }
        let Some((at, candidate, len)) = best else {
            return;
        };

        let sum_sq: f64 = candidate[..len].iter().map(|g| g * g).sum();
        let norm = if sum_sq > POWER_FLOOR {
            1.0 / sum_sq.sqrt()
        } else {
            0.0
        };
        for (&speaker, &gain) in self.tuples[at].speakers().iter().zip(&candidate[..len]) {
            gains[usize::from(speaker)] = (gain * norm).max(0.0);
        }
    }
}

/// Fold a bearing into the front hemisphere by mirroring it about the lateral
/// (left-right) axis: 135 degrees becomes 45, 180 becomes 0.
///
/// Only correct for an array with no rear speaker; see
/// [`SpeakerLayout::gains_into`].
#[inline]
fn fold_to_front(azimuth: Azimuth) -> Azimuth {
    let deg = azimuth.wrap().get();
    if deg.abs() > 90.0 {
        // `Azimuth` deliberately omits arithmetic operators; the mirror is a
        // reflection rather than a rotation, so it is computed on the scalar
        // and rebuilt. `wrap` guarantees `-180..180`, so the result lands in
        // `-90..90` and needs no second wrap.
        Azimuth::new(deg.signum() * (180.0 - deg.abs()))
    } else {
        azimuth.wrap()
    }
}

/// Apply spread to `gains` in place, holding the sum of squares at 1:
/// the energy law the solve establishes, preserved across the diffusion blend
/// rather than newly imposed by it.
#[inline]
fn apply_spread(spread: Spread, gains: &mut [Gain]) {
    // Unwrapped once here: the blend below is interpolation arithmetic on the
    // scalar, which `Spread` deliberately does not define operators for.
    let spread = spread.get();
    if spread <= 0.0 {
        return;
    }
    let equal_gain = 1.0 / (gains.len() as f64).sqrt();
    for gain in gains.iter_mut() {
        *gain = Gain::new(crate::lerp(gain.get(), equal_gain, spread));
    }
    let sum_sq: f64 = gains.iter().map(|g| g.get() * g.get()).sum();
    if sum_sq > 0.0 {
        let norm = 1.0 / sum_sq.sqrt();
        for gain in gains.iter_mut() {
            *gain *= Gain::new(norm);
        }
    }
}

#[cfg(test)]
mod tests;
