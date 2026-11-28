//! The VBAP block kernel: the speaker gains of one direction, and one output
//! channel rendered along a ramp between two of them.
//!
//! The caller reads its controls on the grid ([`COEFF_INTERVAL`](crate::COEFF_INTERVAL) frames, counted
//! from its last reset), hands [`VbapKernel::gains`] the bearing, height, width
//! and spread of the grid point, ramps from the previous grid point's gains to
//! these ([`Ramp`]), and renders each speaker's channel with
//! [`VbapGains::process_channel`].

use tutti_buffers::MAX_CHANNELS;
use tutti_buffers::{Float, Fold, Mono, MonoMut};
use tutti_units::{ArcDegrees, Azimuth, Elevation, Gain, Spread, StereoWidth};

use crate::design::along;
use crate::{Lerp, Ramp};

mod error;
mod layout;
mod math;
mod select;

pub use error::{GainsLengthMismatch, SpeakerGeometryError};
pub use layout::{BedLayout, SpeakerLayout};

/// A width below this folds the stereo pair to mono and pans one source.
const MONO_WIDTH: StereoWidth = StereoWidth::new(0.001);

/// How far each virtual source sits from the bearing at width 1.
const WIDTH_HALF_ANGLE: ArcDegrees = ArcDegrees::new(15.0);

/// The width that puts the two virtual sources at the antipode of the bearing
/// (an offset of half a turn). The solve saturates the width here by convention,
/// so a huge one cannot overflow the offset.
const MAX_WIDTH: StereoWidth = StereoWidth::new(180.0 / WIDTH_HALF_ANGLE.get());

/// One direction's speaker gains, as an end of a ramp between two grid points.
///
/// `mono` records which branch produced them: a mono set is one gain vector
/// (`a`) applied to the folded pair, a stereo set is two: `a` for the left
/// virtual source, `b` for the right. Entries past the speaker count are zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VbapGains {
    mono: bool,
    a: [Gain; MAX_CHANNELS],
    b: [Gain; MAX_CHANNELS],
}

impl VbapGains {
    /// Speaker `s`'s gains as a (left, right) pair, whatever the branch.
    ///
    /// A mono set weighs each channel by half its gain (`fold(l, r) * g` is
    /// `l * g/2 + r * g/2` up to rounding), which is what lets a ramp cross
    /// from one branch to the other without a step.
    #[inline]
    pub fn pair(&self, s: usize) -> (Gain, Gain) {
        if self.mono {
            let half = Gain::new(self.a[s].get() * 0.5);
            (half, half)
        } else {
            (self.a[s], self.b[s])
        }
    }

    /// Render one output channel along `ramp`: speaker `speaker`'s gain, read
    /// at step `first` for the first frame of `out` and one step on for each
    /// frame after, applied to the stereo pair (`left`, `right`) into `out`. No
    /// speaker (the LFE) is silence.
    ///
    /// A frame reads `ramp.at(step)` in closed form (the target from its last
    /// step on), so a segment of frames that starts at step `first` renders the
    /// same bits however it is cut into calls. `first` is one past the steps the
    /// frames before it have used: the first frame after an aim reads step 1,
    /// and the last step of a grid segment is exactly the gains of the grid
    /// point.
    ///
    /// When both ends came from the mono-fold branch the pair is folded
    /// (`mono * g`, `mono` the stereo-to-mono `fold` of each frame);
    /// otherwise each end is expressed as a (left, right) gain pair, so a
    /// width crossing the mono-fold threshold between grid points crossfades
    /// instead of stepping.
    #[inline]
    pub fn process_channel<F: Float>(
        ramp: &Ramp<VbapGains>,
        first: u32,
        speaker: Option<usize>,
        fold: &Fold<F>,
        left: Mono<'_, F>,
        right: Mono<'_, F>,
        mut out: MonoMut<'_, F>,
    ) {
        let (left, right, out) = (left.samples(), right.samples(), out.samples_mut());
        let Some(s) = speaker else {
            out.fill(F::ZERO);
            return;
        };
        let (from, to, n) = (&ramp.from, &ramp.to, ramp.n);
        // The gain at step `k`, `Ramp::at`'s rule (the target from the last step on)
        // for one speaker's pair: `at` would blend all of the set for each frame. The ends
        // cross to `F` once per call and the blend runs in `F`.
        let blend = |from: F, to: F, k: u32| {
            if k >= n {
                to
            } else {
                crate::lerp(from, to, along::<F>(k, n))
            }
        };
        let steps = first..;
        // A pair of ends that are the same bits is a held gain, which skips the
        // blend (`-0.0` and `+0.0` differ in bits, so a sign change is a ramp).
        if from.mono && to.mono {
            let (g_from, g_to) = (from.a[s], to.a[s]);
            let mono = |l: F, r: F| {
                let mut m = [F::ZERO];
                fold.frame(&[l, r], &mut m);
                m[0]
            };
            if g_from.get().to_bits() == g_to.get().to_bits() {
                let g_to = F::from_unit(g_to);
                for ((o, &l), &r) in out.iter_mut().zip(left).zip(right) {
                    *o = mono(l, r) * g_to;
                }
                return;
            }
            let (g_from, g_to) = (F::from_unit(g_from), F::from_unit(g_to));
            for (((o, &l), &r), k) in out.iter_mut().zip(left).zip(right).zip(steps) {
                *o = mono(l, r) * blend(g_from, g_to, k);
            }
        } else {
            let (a_from, b_from) = from.pair(s);
            let (a_to, b_to) = to.pair(s);
            if a_from.get().to_bits() == a_to.get().to_bits()
                && b_from.get().to_bits() == b_to.get().to_bits()
            {
                let (a_to, b_to) = (F::from_unit(a_to), F::from_unit(b_to));
                for ((o, &l), &r) in out.iter_mut().zip(left).zip(right) {
                    *o = l * a_to + r * b_to;
                }
                return;
            }
            let (a_from, a_to) = (F::from_unit(a_from), F::from_unit(a_to));
            let (b_from, b_to) = (F::from_unit(b_from), F::from_unit(b_to));
            for (((o, &l), &r), k) in out.iter_mut().zip(left).zip(right).zip(steps) {
                *o = l * blend(a_from, a_to, k) + r * blend(b_from, b_to, k);
            }
        }
    }
}

/// A ramp between two gain sets: each speaker's (left, right) pair blended
/// linearly, so the ramp is a chord between two unit-power vectors and a
/// mono set crosses to a stereo one without a step. Two mono sets stay mono.
impl Lerp for VbapGains {
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        if self.mono && to.mono {
            let mut a = [Gain::SILENT; MAX_CHANNELS];
            for (g, (from, to)) in a.iter_mut().zip(self.a.iter().zip(&to.a)) {
                *g = Gain::new(from.get().lerp(&to.get(), k, n));
            }
            return Self {
                mono: true,
                a,
                b: [Gain::SILENT; MAX_CHANNELS],
            };
        }
        let (mut a, mut b) = ([Gain::SILENT; MAX_CHANNELS], [Gain::SILENT; MAX_CHANNELS]);
        for s in 0..MAX_CHANNELS {
            let (from, to) = (self.pair(s), to.pair(s));
            a[s] = Gain::new(from.0.get().lerp(&to.0.get(), k, n));
            b[s] = Gain::new(from.1.get().lerp(&to.1.get(), k, n));
        }
        Self { mono: false, a, b }
    }
}

/// A ramp of a bearing is a ramp along the short arc
/// ([`Azimuth::lerp_shortest`]): from 170 to -170 it passes through 180, never
/// through the front.
impl Lerp for Azimuth {
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        self.lerp_shortest(*to, f64::from(k) / f64::from(n))
    }
}

/// A ramp of a height is a line.
impl Lerp for Elevation {
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        Elevation::lerp(*self, *to, f64::from(k) / f64::from(n))
    }
}

/// A speaker layout and the last solve over it.
#[derive(Clone, Debug)]
pub struct VbapKernel {
    layout: SpeakerLayout,
    /// The last [`gains`](Self::gains) result and the input bits it was solved
    /// for (bearing, height, width, spread). The solve is a pure function of
    /// those four, so a held source reuses it instead of running two VBAP
    /// solves per block.
    last_solve: Option<([u64; 4], VbapGains)>,
}

impl VbapKernel {
    /// A kernel over `layout`, with nothing solved yet.
    pub fn new(layout: SpeakerLayout) -> Self {
        Self {
            layout,
            last_solve: None,
        }
    }

    /// The gains for one frame at `(azimuth, elevation)`, already normalized
    /// (bearing wrapped, height and spread clamped).
    ///
    /// `width < 0.001` folds the pair to mono and pans one source; otherwise
    /// the two channels become two virtual sources one `width`-scaled offset
    /// either side of the bearing.
    ///
    /// # Spread applies to both virtual sources
    ///
    /// The `width > 0` branch must reach spread (`SpeakerLayout::gains_into`)
    /// too, or a spread set on a node fed any non-zero width would silently do
    /// nothing. `width` and `spread` are orthogonal: width says how far apart
    /// the two virtual sources sit, spread how far each one is smeared across
    /// the speaker field, so neither may suppress the other. Both go through
    /// [`SpeakerLayout::gains_into`], which also gives each virtual source
    /// unit energy on its own; without that, the width branch would
    /// reintroduce the rear-arc silence the mono branch avoids.
    ///
    /// A direction that is not finite gives silence: every gain zero
    /// ([`SpeakerLayout::gains_into`]).
    pub fn gains(
        &mut self,
        azimuth: Azimuth,
        elevation: Elevation,
        width: StereoWidth,
        spread: Spread,
    ) -> VbapGains {
        let key = [
            azimuth.get().to_bits(),
            elevation.get().to_bits(),
            width.get().to_bits(),
            spread.get().to_bits(),
        ];
        if let Some((solved_for, gains)) = self.last_solve
            && solved_for == key
        {
            return gains;
        }
        let gains = self.solve(azimuth, elevation, width, spread);
        self.last_solve = Some((key, gains));
        gains
    }

    /// [`gains`](Self::gains) without the reuse.
    fn solve(
        &self,
        azimuth: Azimuth,
        elevation: Elevation,
        width: StereoWidth,
        spread: Spread,
    ) -> VbapGains {
        // A NaN reads as no width.
        let width = if width.get().is_nan() {
            StereoWidth::new(0.0)
        } else {
            width.clamp(StereoWidth::new(0.0), MAX_WIDTH)
        };
        let count = self.layout.speaker_count();
        if width < MONO_WIDTH {
            let mut a = [Gain::SILENT; MAX_CHANNELS];
            self.layout
                .gains_into(azimuth, elevation, spread, &mut a[..count])
                .expect("the slice is the layout's speaker count");
            return VbapGains {
                mono: true,
                a,
                b: [Gain::SILENT; MAX_CHANNELS],
            };
        }
        let (left, right) = virtual_sources(azimuth, width);
        let (mut a, mut b) = ([Gain::SILENT; MAX_CHANNELS], [Gain::SILENT; MAX_CHANNELS]);
        self.layout
            .gains_into(left, elevation, spread, &mut a[..count])
            .expect("the slice is the layout's speaker count");
        self.layout
            .gains_into(right, elevation, spread, &mut b[..count])
            .expect("the slice is the layout's speaker count");
        VbapGains { mono: false, a, b }
    }
}

/// The two virtual sources, one `width`-scaled offset either side of the
/// bearing. `rotate_by`, not `+`: the sum crosses the seam. At azimuth 170 with
/// full width the left source is at 185, which *is* -175, the same wraparound
/// the node normalizes the target to, re-introduced here by adding to the
/// already-wrapped value.
fn virtual_sources(azimuth: Azimuth, width: StereoWidth) -> (Azimuth, Azimuth) {
    let offset = WIDTH_HALF_ANGLE * width.get();
    (azimuth.rotate_by(offset), azimuth.rotate_by(-offset))
}

#[cfg(test)]
mod tests;
