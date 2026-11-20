//! The binaural convolver: overlap-save FFT convolution against the HRIR
//! blended for the source's direction.
//!
//! ## Steps
//!
//! Input accumulates into steps of [`HRTF_STEP`] frames. When a step is full it is
//! rendered in one go and its output drains over the next step, so output
//! lags input by exactly [`HRTF_LATENCY`] = `HRTF_STEP` frames whatever the block size.
//! A step costs one real forward FFT of the input history and, per ear, one
//! real inverse FFT of the product with the blended spectrum (see
//! [`HrtfSet`]), plus another per ear while the direction moves. The
//! transforms are [`crate::RealFft`]'s, so the arithmetic does not
//! depend on the CPU.
//!
//! ## Direction changes
//!
//! The caller de-zippers the commanded position and hands the convolver the
//! direction to render once per step (the `aim` closure of
//! [`HrtfBinaural::process`], called once for each step that completes).
//! Each step's direction is blended across the sphere face it
//! crosses (barycentric weights over the face's three vertices). When the
//! blend differs from the previous step's, the step is rendered through both
//! filters and crossfaded linearly across it, so the output moves from one
//! filter to the next with no jump. Overlap-save keeps no filter state
//! between steps (each step's output is computed from the input history
//! alone), so both renders are exact and the crossfade is all there is to a
//! change.

use std::sync::Arc;

use crate::FftFloat;
use crate::design::along;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Fold, Mono, MonoMut, Planes, StereoPlanes};
use tutti_units::{Direction, Mix, Radians, SampleRate};

mod hull;
mod sphere;

use crate::Complex;
pub use sphere::{HrirMeasurement, HrirSphere};
use sphere::{HrtfSet, Weights};

/// Frames per convolution step, and the crossfade length of a direction
/// change.
pub const HRTF_STEP: usize = 128;

/// How many frames output lags input: what the node reports to PDC.
///
/// A sample written at position `k` of a step leaves when position `k` of the
/// next step is read, `HRTF_STEP` frames later. The HRIR's own onset (the
/// ear-to-ear delay that *is* the spatial cue) is signal, like a convolution
/// reverb's pre-delay, and is not latency.
pub const HRTF_LATENCY: usize = HRTF_STEP;

/// The error returned when an [`HrirSphere`]
/// cannot be built from its HRIR data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HrtfBinauralError {
    /// The bytes do not start with the `HRIR` magic.
    #[error("not an HRIR sphere: the bytes do not start with `HRIR`")]
    NotASphere,
    /// The bytes end before the sphere their header describes.
    #[error("HRIR sphere truncated: {needed} bytes needed, {found} present")]
    Truncated {
        /// Bytes the header implies (saturating at `usize::MAX`).
        needed: usize,
        /// Bytes given.
        found: usize,
    },
    /// The bytes run past the sphere their header describes.
    #[error("HRIR sphere has trailing bytes: {size} bytes describe it, {found} present")]
    Trailing {
        /// Bytes the header implies.
        size: usize,
        /// Bytes given.
        found: usize,
    },
    /// The header describes no usable sphere: a zero length or vertex count,
    /// or an index count that is not a whole number of triangles.
    #[error("bad HRIR sphere header: {0}")]
    Header(&'static str),
    /// The rate a sphere is built at is not a whole number of hertz, which
    /// the byte format cannot store.
    #[error("HRIR sphere rate {} Hz is not a whole number of hertz", .rate.get())]
    FractionalRate {
        /// The rate given.
        rate: SampleRate,
    },
    /// The sphere's sample rate is outside [`MIN_SPHERE_RATE`] to
    /// [`MAX_SPHERE_RATE`] Hz. Resampling scales the response length by the
    /// ratio of the rates, so the bound keeps a file from asking for a
    /// response thousands of times its own size.
    #[error("HRIR sphere rate {rate} Hz is outside {MIN_SPHERE_RATE}..={MAX_SPHERE_RATE} Hz")]
    Rate {
        /// The rate the header gives, in Hz.
        rate: u32,
    },
    /// The responses last longer than 50 ms at the sphere's rate
    /// (`len > rate / 20`). Measured HRIRs last a few milliseconds. The
    /// bound keeps what preparing the sphere allocates and computes per
    /// vertex in proportion to an HRIR: a set prepared at any rate holds at
    /// most 50 ms of frames there, rounded up.
    #[error("HRIR responses are {len} frames, past the {max} (50 ms) the sphere's rate allows")]
    ResponseTooLong {
        /// The response length the header gives, in frames.
        len: usize,
        /// The longest the sphere's rate allows, in frames.
        max: usize,
    },
    /// A face names a vertex the sphere does not have.
    #[error("face {face} names vertex {vertex}, but the sphere has {vertices} vertices")]
    FaceIndex {
        /// The face's index.
        face: usize,
        /// The vertex index it names.
        vertex: u32,
        /// How many vertices the sphere has.
        vertices: usize,
    },
    /// A vertex's distance from the origin is outside `1e-6..=1e6`, or one
    /// of its response samples is non-finite or larger in magnitude than
    /// `1e6`. (A position too close to the origin normalizes to NaN; a
    /// sample near `f32::MAX` overflows the FFT.)
    #[error("vertex {vertex} is too near or far from the origin, or holds an out-of-range value")]
    Vertex {
        /// The vertex's index.
        vertex: usize,
    },
    /// The responses of a measurement are not as long as the first's.
    #[error("measurement {vertex} has a response of {found} frames, but the first has {expected}")]
    ResponseLength {
        /// The measurement's index.
        vertex: usize,
        /// The first measurement's length, in frames.
        expected: usize,
        /// The length found, in frames.
        found: usize,
    },
    /// Two measurements are in one direction.
    #[error("measurements {first} and {second} are in the same direction")]
    Duplicate {
        /// The earlier measurement's index.
        first: usize,
        /// The later one's.
        second: usize,
    },
    /// There are fewer than four directions, or they all lie in one plane
    /// through the listener: no triangles enclose the listener.
    #[error("the directions are fewer than four or coplanar, so they do not enclose the listener")]
    NotEnclosing,
    /// A direction lies inside the hull of the others (closer to the
    /// listener than the triangle it would sit on, after rounding), so no
    /// face blends its responses.
    #[error("measurement {vertex}'s direction lies inside the hull of the others")]
    Inside {
        /// The measurement's index.
        vertex: usize,
    },
    /// No face is a usable triangle: there are none, or each one's plane
    /// passes through the origin, so no direction can be blended.
    #[error("the HRIR sphere has no usable face")]
    NoFace,
}

/// The lowest sphere rate `HrirSphere::from_bytes`
/// accepts, in Hz.
pub const MIN_SPHERE_RATE: u32 = 8_000;

/// The highest sphere rate `HrirSphere::from_bytes`
/// accepts, in Hz.
pub const MAX_SPHERE_RATE: u32 = 384_000;

/// The highest rate the sphere is prepared at, in Hz. A node built or
/// prepared above it renders with the set prepared at this rate, so a
/// prepared response is at most `MAX_PREPARED_RATE / 20` frames (50 ms
/// here) whatever rate the graph asks for: an unbounded one (`1e10`) once
/// sized each vertex's spectrum at 2^26 values and aborted on allocation.
/// 768 kHz is twice the highest rate audio hardware runs at. Above it the
/// HRIR is applied time-compressed by `rate / 768 kHz`, which shifts its
/// spectral cues up by that factor; the de-zipper still ramps in the
/// graph's own time.
pub const MAX_PREPARED_RATE: u32 = 768_000;

/// The sphere's rate vocabulary: an integral Hz, as the file stores it. It is
/// also the identity a rebuild compares on (an integer equality, not a float
/// one); no HRIR dataset ships at a fractional rate. At least 1 Hz, so a
/// degenerate rate still prepares a (useless but well-formed) set, and at
/// most [`MAX_PREPARED_RATE`]. A NaN rate prepares at 1 Hz.
fn rate_key(sample_rate: SampleRate) -> u32 {
    // `max` before `min`, not `clamp`: `max` takes 1 over a NaN.
    sample_rate
        .get()
        .round()
        .max(1.0)
        .min(f64::from(MAX_PREPARED_RATE)) as u32
}

/// The streaming convolver for one source.
///
/// A clone is [`fresh`](Self::fresh): it takes the direction to aim at.
pub struct HrtfBinaural<F: FftFloat = f32> {
    sphere: Arc<HrirSphere>,
    set: Arc<HrtfSet<F>>,
    /// The input history, `fft_len` frames: the last `fft_len - HRTF_STEP` frames
    /// already rendered, then the step being filled.
    history: Vec<F>,
    /// Frames of the current step filled so far, `0..HRTF_STEP`.
    fill: usize,
    /// The last rendered step, drained while the next one fills: the wet
    /// pair and the mono input it was rendered from (the dry path, equally
    /// late).
    wet_left: Vec<F>,
    wet_right: Vec<F>,
    dry: Vec<F>,
    /// The input history's spectrum, `fft_len / 2 + 1` bins.
    spectrum: Vec<Complex<F>>,
    /// The product with one ear's filter, inverse-transformed in place.
    product: Vec<Complex<F>>,
    /// One ear's `fft_len` output frame, and the previous filter's while a
    /// direction change crossfades.
    frame: Vec<F>,
    frame_prev: Vec<F>,
    scratch: Vec<Complex<F>>,
    /// The blend the last step ended on.
    current: Weights,
    face_hint: usize,
    /// The input pair folded to the mono the renderer convolves.
    fold: Fold<F>,
}

impl<F: FftFloat> std::fmt::Debug for HrtfBinaural<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HrtfBinaural")
            .field("fill", &self.fill)
            .field("face_hint", &self.face_hint)
            .finish_non_exhaustive()
    }
}

impl<F: FftFloat> HrtfBinaural<F> {
    /// Prepares `sphere` at `sample_rate`, aimed at `aim`. Control thread.
    ///
    /// The sphere is shared: a renderer and its [`fresh`](Self::fresh) copies read the one
    /// parsed [`HrirSphere`].
    #[must_use]
    pub fn new(sphere: Arc<HrirSphere>, sample_rate: SampleRate, aim: Direction) -> Self {
        let set = Arc::new(HrtfSet::build(&sphere, rate_key(sample_rate), HRTF_STEP));
        Self::over(sphere, set, aim)
    }

    /// A renderer over a prepared set, with cleared streaming state, aimed
    /// at `aim`.
    fn over(sphere: Arc<HrirSphere>, set: Arc<HrtfSet<F>>, aim: Direction) -> Self {
        let n = set.fft_len;
        let bins = set.fft.bins();
        let zero = Complex::new(F::ZERO, F::ZERO);
        let mut face_hint = 0;
        let current = set.weights(unit_vector(aim), &mut face_hint);
        Self {
            history: vec![F::ZERO; n],
            fill: 0,
            wet_left: vec![F::ZERO; HRTF_STEP],
            wet_right: vec![F::ZERO; HRTF_STEP],
            dry: vec![F::ZERO; HRTF_STEP],
            spectrum: vec![zero; bins],
            product: vec![zero; bins],
            frame: vec![F::ZERO; n],
            frame_prev: vec![F::ZERO; n],
            scratch: set.fft.new_scratch(),
            current,
            face_hint,
            fold: Fold::between(
                &ChannelTopology::default_for(ChannelLayout::STEREO),
                &ChannelTopology::default_for(ChannelLayout::MONO),
            )
            .expect("stereo folds to mono"),
            sphere,
            set,
        }
    }

    /// Frames that outlive a silent input once the [`HRTF_LATENCY`] is
    /// compensated: the response spreads a sample `ir_len - 1` frames past
    /// the frame it leaves at. A tail excludes latency, so the step's delay
    /// is not counted here. The length is fixed at a rate, so this is exact.
    pub fn ring_out(&self) -> usize {
        self.set.ir_len - 1
    }

    /// Prepares the sphere at a new rate and clears the streaming state,
    /// aimed at what `aim` returns. At the set's current rate it does
    /// nothing (allocation-free, `aim` not called), so a re-prepare for a new
    /// block size keeps the tail in flight. Two graph rates above
    /// [`MAX_PREPARED_RATE`] share one set. Allocates on a new rate: control
    /// thread only.
    pub fn prepare(&mut self, sample_rate: SampleRate, aim: impl FnOnce() -> Direction) {
        let rate = rate_key(sample_rate);
        if rate == self.set.rate {
            return;
        }
        let set = HrtfSet::build(&self.sphere, rate, HRTF_STEP);
        *self = Self::over(self.sphere.clone(), Arc::new(set), aim());
    }

    /// Zero the streaming buffers and aim at `aim`, so the next step renders
    /// there with no crossfade in from elsewhere.
    pub fn reset_state(&mut self, aim: Direction) {
        self.history.fill(F::ZERO);
        self.fill = 0;
        self.wet_left.fill(F::ZERO);
        self.wet_right.fill(F::ZERO);
        self.dry.fill(F::ZERO);
        self.current = self.set.weights(unit_vector(aim), &mut self.face_hint);
    }

    /// Shares the parsed sphere and its prepared set (nothing is reparsed or
    /// re-planned), with cleared streaming state aimed at `aim`. Allocates
    /// the streaming buffers: control thread.
    #[must_use]
    pub fn fresh(&self, aim: Direction) -> Self {
        Self::over(self.sphere.clone(), self.set.clone(), aim)
    }

    /// Renders a block: the input pair is folded to mono, and each output is
    /// `blend` between the delayed dry mono and the binaural pair. `aim` is
    /// called once for each step the block completes and returns the
    /// direction that step renders. Allocation-free.
    pub fn process(
        &mut self,
        blend: Mix,
        input: StereoPlanes<'_, F>,
        output: (MonoMut<'_, F>, MonoMut<'_, F>),
        mut aim: impl FnMut() -> Direction,
    ) {
        let input = (input.left(), input.right());
        let (output, output_right) = output;
        let output = (output.into_samples_mut(), output_right.into_samples_mut());
        let mix = F::from_unit(blend);
        let frames = output.0.len();
        let start = self.history.len() - HRTF_STEP;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(HRTF_STEP - self.fill);
            let at = self.fill;
            let ins = (&input.0[done..done + n], &input.1[done..done + n]);
            let outs = (&mut output.0[done..done + n], &mut output.1[done..done + n]);
            let wet = (&self.wet_left[at..at + n], &self.wet_right[at..at + n]);
            let dry = &self.dry[at..at + n];
            for (((l, r), &d), (&wl, &wr)) in outs
                .0
                .iter_mut()
                .zip(&mut *outs.1)
                .zip(dry)
                .zip(wet.0.iter().zip(wet.1))
            {
                *l = crate::lerp(d, wl, mix);
                *r = crate::lerp(d, wr, mix);
            }
            let fresh = &mut self.history[start + at..start + at + n];
            let pair = [ins.0, ins.1];
            let folded = Planes::new(&pair).and_then(|pair| {
                self.fold
                    .fold_planar_channel(pair, 0, MonoMut::new(&mut *fresh))
            });
            debug_assert!(folded.is_ok(), "a step's planes are one length: {folded:?}");
            if folded.is_err() {
                // Not folded: silent, never the history's previous contents.
                fresh.fill(F::ZERO);
            }
            self.fill += n;
            done += n;
            if self.fill == HRTF_STEP {
                self.render_step(aim());
                self.fill = 0;
            }
        }
    }

    /// Convolve the full step into the wet buffers and slide the history.
    fn render_step(&mut self, aim: Direction) {
        let next = self.set.weights(unit_vector(aim), &mut self.face_hint);
        let set = &*self.set;
        let start = set.fft_len - HRTF_STEP;
        self.dry.copy_from_slice(&self.history[start..]);

        set.fft.forward(
            Mono::new(&self.history),
            &mut self.spectrum,
            &mut self.scratch,
        );

        let crossfade = next != self.current;
        let wet = [&mut self.wet_left, &mut self.wet_right];
        for (ear, wet) in wet.into_iter().enumerate() {
            blend_into(
                set,
                &next,
                ear,
                &self.spectrum,
                &mut self.product,
                &mut self.frame,
                &mut self.scratch,
            );
            let new = &self.frame[start..];
            if crossfade {
                blend_into(
                    set,
                    &self.current,
                    ear,
                    &self.spectrum,
                    &mut self.product,
                    &mut self.frame_prev,
                    &mut self.scratch,
                );
                let old = &self.frame_prev[start..];
                for (i, ((w, &y), &z)) in wet.iter_mut().zip(new).zip(old).enumerate() {
                    // Reaches the new filter on the step's last frame, so the
                    // next step continues from it.
                    let t = along::<F>(i as u32 + 1, HRTF_STEP as u32);
                    *w = crate::lerp(z, y, t);
                }
            } else {
                wet.copy_from_slice(new);
            }
        }
        self.current = next;
        self.history.copy_within(HRTF_STEP.., 0);
    }
}

/// One ear's `fft_len` output frame for `input`'s spectrum through the
/// filter `weights` blend: the weighted sum of the three vertices' spectra,
/// multiplied in and inverse-transformed. `product` is clobbered.
fn blend_into<F: FftFloat>(
    set: &HrtfSet<F>,
    weights: &Weights,
    ear: usize,
    input: &[Complex<F>],
    product: &mut [Complex<F>],
    frame: &mut [F],
    scratch: &mut [Complex<F>],
) {
    let [a, b, c] = weights.verts.map(|v| {
        let (left, right) = set.spectrum(v);
        if ear == 0 { left } else { right }
    });
    let [wa, wb, wc] = weights.w.map(F::from_design);
    let filters = a.iter().zip(b).zip(c);
    for ((p, &x), ((&a, &b), &c)) in product.iter_mut().zip(input).zip(filters) {
        *p = x * (a * wa + b * wb + c * wc);
    }
    set.fft.inverse(product, MonoMut::new(frame), scratch);
}

/// Map azimuth/elevation in degrees to a unit direction, in the sphere's
/// right-handed frame: +x right, +y up, +z toward the listener's back.
///
/// Convention (matching the rest of `spatial`): azimuth 0 = front, +90 = left;
/// elevation 0 = ear level, +90 = up.
#[inline]
fn unit_vector(aim: Direction) -> [f64; 3] {
    let az = Radians::from(aim.azimuth);
    let el = Radians::from(aim.elevation);
    let cos_el = el.cos();
    // +90° azimuth (left) → -x; front (0°) → -z.
    [-az.sin() * cos_el, el.sin(), -az.cos() * cos_el]
}

#[cfg(test)]
mod tests;
