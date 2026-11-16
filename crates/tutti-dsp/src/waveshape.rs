//! Memoryless waveshaping curves ([`ShapeKind`]): `Tanh`, `Atan`, `Softsign`,
//! `HardClip`, `Crush` and `SoftCrush`, each one line of arithmetic, and the
//! [`WaveshaperBank`] that runs one over a block.
//!
//! A bank's drive is a [`Glide`] over [`GAIN_GLIDE`], or a lane read per frame
//! where a port drives it; a frame whose lane value is not finite reads the
//! aimed drive. Frame `i` reads the glide at step `taken + 1 + i`, then the
//! block's steps are taken.

use tutti_buffers::{Float, Mono, Planes, PlanesMut, Sample};
use tutti_types::{Drive, SampleRate};

use crate::{GAIN_GLIDE, Glide, Steps};

/// The frames a pass over a block takes at a time: the hardness of a stretch is held in a
/// stack array.
const CHUNK: usize = 64;

/// Selects which waveshaping curve a [`WaveshaperBank`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum ShapeKind {
    /// `tanh` saturation — smooth, tube-like.
    #[default]
    Tanh,
    /// `atan` saturation — slightly harder knee than tanh.
    Atan,
    /// `softsign` (`x / (1 + |x|)`) — gentle.
    Softsign,
    /// Hard clip to ±1.
    HardClip,
    /// Bitcrush-style staircase quantization.
    Crush,
    /// Smoothed staircase quantization.
    SoftCrush,
}

/// Runs `$body` with `$k` bound to `$kind` as a **constant**, one arm per
/// kind, so each arm's loop is compiled for its own curve: the `match` in
/// [`ShapeKind::curve`] folds away instead of running per sample.
macro_rules! with_kind {
    ($kind:expr, |$k:ident| $body:expr) => {
        match $kind {
            ShapeKind::Tanh => {
                const $k: ShapeKind = ShapeKind::Tanh;
                $body
            }
            ShapeKind::Atan => {
                const $k: ShapeKind = ShapeKind::Atan;
                $body
            }
            ShapeKind::Softsign => {
                const $k: ShapeKind = ShapeKind::Softsign;
                $body
            }
            ShapeKind::HardClip => {
                const $k: ShapeKind = ShapeKind::HardClip;
                $body
            }
            ShapeKind::Crush => {
                const $k: ShapeKind = ShapeKind::Crush;
                $body
            }
            ShapeKind::SoftCrush => {
                const $k: ShapeKind = ShapeKind::SoftCrush;
                $body
            }
        }
    };
}

impl ShapeKind {
    /// Shapes one sample: `x` through this curve at `drive`.
    ///
    /// `drive` is input gain into the curve, floored at 0. For the two
    /// staircases it is instead the number of **levels per unit**, floored at 1
    /// (fewer than one level per unit is not a staircase).
    ///
    /// Memoryless and total: every curve maps every finite `x` to a finite
    /// output, and none keeps state between calls.
    #[inline]
    #[must_use]
    pub fn apply(self, x: f32, drive: Drive) -> f32 {
        self.curve(self.hardness(drive.get()), x)
    }

    /// The hardness the curve runs at for `drive`, floored the way the kind needs.
    #[inline(always)]
    fn hardness(self, drive: f32) -> f32 {
        let drive = drive.max(0.0);
        match self {
            Self::Crush | Self::SoftCrush => drive.max(1.0),
            _ => drive,
        }
    }

    /// The curve at a hardness from [`hardness`](Self::hardness).
    ///
    /// The operation order within each arm is fixed: the pinned values in the
    /// tests depend on it bit for bit.
    #[inline(always)]
    fn curve(self, hardness: f32, x: f32) -> f32 {
        use core::f32::consts::PI;
        match self {
            Self::Tanh => (x * hardness).tanh(),
            // Rescaled to saturate at ±1 while keeping unit slope at the origin.
            Self::Atan => (x * (hardness * PI * 0.5)).atan() * (2.0 / PI),
            Self::Softsign => {
                let y = x * hardness;
                y / (1.0 + y.abs())
            }
            Self::HardClip => (x * hardness).clamp(-1.0, 1.0),
            Self::Crush => (x * hardness).round() / hardness,
            Self::SoftCrush => {
                let y = x * hardness;
                let step = y.floor();
                (step + smooth9(y - step)) / hardness
            }
        }
    }

    /// `input` through the curve at one `hardness`, into `output`. The loop is compiled
    /// for the kind, so the curve inlines.
    #[inline(always)]
    fn shape_held<S: Float>(self, hardness: f32, input: &[S], output: &mut [S]) {
        with_kind!(self, |K| {
            for (o, x) in output.iter_mut().zip(input) {
                *o = K.curve(hardness, x.convert_to::<f32>()).convert_to::<S>();
            }
        })
    }

    /// `input` through the curve at a hardness per frame, into `output`.
    #[inline(always)]
    fn shape_each<S: Float>(self, hardness: &[f32], input: &[S], output: &mut [S]) {
        with_kind!(self, |K| {
            for ((o, x), &h) in output.iter_mut().zip(input).zip(hardness) {
                *o = K.curve(h, x.convert_to::<f32>()).convert_to::<S>();
            }
        })
    }
}

/// The ninth-order smoothstep: `0 → 0`, `1 → 1`, with the first four
/// derivatives zero at both ends, so each [`ShapeKind::SoftCrush`] step is a
/// smooth S rather than a jump.
#[inline]
fn smooth9(x: f32) -> f32 {
    let x2 = x * x;
    ((((70.0 * x - 315.0) * x + 540.0) * x - 420.0) * x + 126.0) * x2 * x2 * x
}

/// The controls of a [`WaveshaperBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveshaperControls {
    /// Input gain into the curve, or the levels per unit of a staircase; the bank floors it
    /// as [`ShapeKind::apply`] does.
    pub drive: Drive,
}

impl WaveshaperControls {
    /// Unity drive: what a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        drive: Drive::UNITY,
    };
}

/// A waveshaper of any width: one curve ([`ShapeKind`]) at one drive shared by every
/// channel. The channels keep no state, so each is shaped alone.
///
/// Built on the control side for one rate; nothing it holds grows.
#[derive(Clone, Copy, Debug)]
pub struct WaveshaperBank {
    kind: ShapeKind,
    /// The drive, gliding over [`GAIN_GLIDE`].
    drive: Glide<f32>,
    /// The steps a drive glide takes: [`GAIN_GLIDE`] at the rate.
    steps: u32,
}

impl WaveshaperBank {
    /// A waveshaper of `kind` at `rate`, at unity drive, to be aimed.
    #[must_use]
    pub fn new(kind: ShapeKind, rate: SampleRate) -> Self {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates rather
        // than panic, since a prepare must not.
        let steps = u32::try_from(GAIN_GLIDE.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            kind,
            drive: Glide::landed(WaveshaperControls::DEFAULT.drive.get()),
            steps,
        }
    }

    /// Lands the drive on its target now: back to rest.
    pub fn settle(&mut self) {
        self.drive.settle();
    }

    /// Aims at `controls`: the drive glides over [`GAIN_GLIDE`]. No allocation.
    pub fn aim(&mut self, controls: &WaveshaperControls) {
        self.drive
            .aim_by(controls.drive.get(), Steps::Fixed(self.steps));
    }

    /// Runs `input` into `output`, each channel through the curve at the drive.
    ///
    /// `lane` is the drive per frame where a port drives it: a frame past its end, or one
    /// that is not finite, reads the aimed drive. Channel `c` of `input` feeds channel `c`
    /// of `output`; a channel past the narrower of the two views is not touched, and the
    /// frames rendered are the fewer of the two views'. No allocation.
    pub fn process<S: Float>(
        &mut self,
        lane: Option<Mono<'_, S>>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels().min(output.channels());
        let kind = self.kind;
        let blended = self.drive.blended(frames);
        let taken = self.drive.taken();
        if lane.is_none() && blended == 0 {
            let hardness = kind.hardness(self.drive.target());
            for c in 0..width {
                let mut plane = output.plane_mut(c);
                kind.shape_held(
                    hardness,
                    &input.plane(c).samples()[..frames],
                    &mut plane.samples_mut()[..frames],
                );
            }
        } else {
            let mut hardness = [0.0f32; CHUNK];
            let mut start = 0;
            while start < frames {
                let end = (start + CHUNK).min(frames);
                for (k, h) in hardness[..end - start].iter_mut().enumerate() {
                    let i = start + k;
                    let drive = lane
                        .and_then(|l| l.samples().get(i))
                        .map(|x| x.convert_to::<f32>())
                        .filter(|d| d.is_finite())
                        .unwrap_or_else(|| {
                            let step = taken.saturating_add(1).saturating_add(i as u32);
                            self.drive.at(step)
                        });
                    *h = kind.hardness(drive);
                }
                for c in 0..width {
                    let mut plane = output.plane_mut(c);
                    kind.shape_each(
                        &hardness[..end - start],
                        &input.plane(c).samples()[start..end],
                        &mut plane.samples_mut()[start..end],
                    );
                }
                start = end;
            }
        }
        self.drive
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests;
