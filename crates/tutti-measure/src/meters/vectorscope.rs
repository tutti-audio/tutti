//! Vectorscope (goniometer) data: a stereo pair as a stream of points.
//!
//! The scope draws each frame at `(side, mid)`, the left/right plane rotated
//! by 45 degrees so mono falls on the vertical axis, a polarity-inverted pair on
//! the horizontal, and a signal in one channel on a diagonal. The rotation is
//! orthonormal, so a point's distance from the origin is the frame's own level
//! `sqrt(l^2 + r^2)`.
//!
//! This is the data only: drawing, persistence and fading are the display's.

use core::marker::PhantomData;
use core::num::NonZeroUsize;

use tutti_buffers::{Sample, StereoPlanes};

use crate::{Measure, MeterError, StreamMeter};

/// One frame on the scope.
///
/// Signed sample values, not unit quantities: they are audio, like the
/// `Mono<S>` and `StereoPlanes<S>` samples they come from.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScopePoint {
    /// `(l - r) / sqrt(2)`: the horizontal axis.
    pub side: f64,
    /// `(l + r) / sqrt(2)`: the vertical axis.
    pub mid: f64,
}

impl ScopePoint {
    /// The frame at left `l` and right `r`.
    #[inline]
    #[must_use]
    pub(crate) fn from_lr(l: f64, r: f64) -> Self {
        Self {
            side: (l - r) * core::f64::consts::FRAC_1_SQRT_2,
            mid: (l + r) * core::f64::consts::FRAC_1_SQRT_2,
        }
    }
}

/// How a stereo pair is decimated into scope points: every `decimate`-th
/// frame is kept, and `1` keeps every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorscopeConfig {
    decimate: NonZeroUsize,
}

impl VectorscopeConfig {
    /// A scope that keeps every `decimate`-th frame.
    #[must_use]
    pub fn new(decimate: NonZeroUsize) -> Self {
        Self { decimate }
    }

    /// The meter at the start of a stream: the infallible core of
    /// [`Measure::measure`], which `vectorscope_points` calls so its own result
    /// needs no `Result`.
    fn bind<S: Sample>(&self) -> VectorscopeMeter<S> {
        VectorscopeMeter {
            decimate: self.decimate,
            skip: 0,
            sample: PhantomData,
        }
    }
}

impl<S: Sample> Measure<S> for VectorscopeConfig {
    type Context = ();
    type Meter = VectorscopeMeter<S>;

    /// A meter at the start of a stream: the first frame is kept. Never fails.
    fn measure(&self, (): ()) -> Result<VectorscopeMeter<S>, MeterError> {
        Ok(self.bind())
    }
}

/// A running vectorscope: where the stride stands between blocks.
///
/// Every push yields its points as items, and there is no output: the points
/// are the answer.
///
/// Carries how many frames to skip before the next point, so a stream fed in
/// blocks keeps one stride phase instead of restarting it per block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorscopeMeter<S: Sample> {
    decimate: NonZeroUsize,
    skip: usize,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> VectorscopeMeter<S> {
    /// Extends `out` with every `decimate`-th frame of the next block.
    fn scan(&mut self, planes: StereoPlanes<'_, S>, out: &mut impl Extend<ScopePoint>) {
        let frames = planes.frames().get();
        let (left, right) = (planes.left(), planes.right());
        let mut at = self.skip;
        while at < frames {
            out.extend([ScopePoint::from_lr(
                left[at].convert_to::<f64>(),
                right[at].convert_to::<f64>(),
            )]);
            at += self.decimate.get();
        }
        self.skip = at - frames;
    }

    /// Starts the stride over: the next frame is kept.
    pub fn reset(&mut self) {
        self.skip = 0;
    }
}

impl<S: Sample> StreamMeter for VectorscopeMeter<S> {
    type Input<'a> = StereoPlanes<'a, S>;
    type Item = ScopePoint;
    type Output = ();

    /// Extends `out` with every `decimate`-th frame of the next block as a
    /// [`ScopePoint`].
    ///
    /// Writes into the caller's collection, so a UI feeding blocks reuses one
    /// `Vec` (clear it between draws). It grows `out` and so allocates when it
    /// is full: not for the audio thread.
    fn push(
        &mut self,
        planes: StereoPlanes<'_, S>,
        out: &mut impl Extend<ScopePoint>,
    ) -> Result<(), MeterError> {
        self.scan(planes, out);
        Ok(())
    }

    fn finish(self) {}
}

/// Every `decimate`-th frame of one whole pair as a [`ScopePoint`], starting
/// at the first: a [`VectorscopeMeter`] fed the pair. Allocates the result.
pub fn vectorscope_points<S: Sample>(
    cfg: &VectorscopeConfig,
    planes: StereoPlanes<'_, S>,
) -> Vec<ScopePoint> {
    let mut out = Vec::with_capacity(planes.frames().get().div_ceil(cfg.decimate.get()));
    cfg.bind().scan(planes, &mut out);
    out
}

#[cfg(test)]
mod tests;
