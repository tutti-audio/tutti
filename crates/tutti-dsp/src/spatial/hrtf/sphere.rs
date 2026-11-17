//! The measured HRIR sphere: built from measurements or parsed from its bytes
//! ([`HrirSphere`]), then prepared per sample rate into the spectra and face
//! table the convolver reads ([`HrtfSet`]).
//!
//! The byte format is `"HRIR"`, then little-endian `u32` source rate,
//! response length, vertex count and index count, the face indices, and per
//! vertex its position followed by its left and right responses as `f32`s. A
//! position is a vector from the listener in the renderer's frame (+x right,
//! +y up, +z behind), whose length does not matter; the faces are triangles
//! wound counter-clockwise seen from outside.

use rustfft::FftNum;
use tutti_buffers::{Float, Mono, Sample};
use tutti_types::{Direction, SampleRate};

use crate::{Complex, CosineWindow, RealFft, WindowedSinc};

use super::hull::convex_hull;
use super::{HrtfBinauralError, MAX_SPHERE_RATE, MIN_SPHERE_RATE, unit_vector};

/// The range a vertex's distance from the origin must fall in. Closer, its
/// direction does not survive normalization in `f32`.
const MIN_DISTANCE: f64 = 1e-6;
const MAX_DISTANCE: f64 = 1e6;

/// The squared distance between two unit directions below which they are
/// one direction.
const DUPLICATE: f64 = 1e-12;

/// The largest response sample magnitude accepted. A spectrum sums up to
/// `fft_len` of them, so a sample near `f32::MAX` would overflow it.
const MAX_SAMPLE: f32 = 1e6;

/// One measured direction and the two ears' impulse responses from it, in
/// sample format `S`; the sphere keeps them as `f32`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HrirMeasurement<'a, S: Sample = f32> {
    direction: Direction,
    left: Mono<'a, S>,
    right: Mono<'a, S>,
}

impl<'a, S: Sample> HrirMeasurement<'a, S> {
    /// The responses `left` and `right` as measured with the source at
    /// `direction`, in the listener's frame of [`Direction`].
    #[must_use]
    pub const fn new(direction: Direction, left: Mono<'a, S>, right: Mono<'a, S>) -> Self {
        Self {
            direction,
            left,
            right,
        }
    }
}

/// A measured head-related impulse response set: the responses of both ears
/// for directions around the listener, at the rate they were measured at, and
/// the triangles that blend them.
///
/// Immutable once built: every renderer, clone and fork built from the same
/// bytes shares one behind an `Arc`.
#[derive(PartialEq)]
pub struct HrirSphere {
    /// The file's own rate, an integral Hz as the format stores it.
    rate: u32,
    /// Response length in frames at `rate`, the same for every vertex.
    len: usize,
    positions: Vec<[f32; 3]>,
    faces: Vec<[u32; 3]>,
    /// Per vertex `v`, its left response at `2v * len` and its right at
    /// `(2v + 1) * len`.
    responses: Vec<f32>,
}

impl std::fmt::Debug for HrirSphere {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HrirSphere")
            .field("rate", &self.rate)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// A little-endian cursor that reports a short read as an error, never a
/// panic.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn word(&mut self) -> Result<[u8; 4], HrtfBinauralError> {
        let word = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|w| <[u8; 4]>::try_from(w).ok())
            .ok_or(HrtfBinauralError::Truncated {
                needed: self.at + 4,
                found: self.bytes.len(),
            })?;
        self.at += 4;
        Ok(word)
    }

    fn u32(&mut self) -> Result<u32, HrtfBinauralError> {
        self.word().map(u32::from_le_bytes)
    }

    fn f32(&mut self) -> Result<f32, HrtfBinauralError> {
        self.word().map(f32::from_le_bytes)
    }
}

impl HrirSphere {
    /// Builds a sphere from measurements: the directions are triangulated by
    /// their convex hull (the faces the renderer blends across), and the
    /// result passes every check [`from_bytes`](Self::from_bytes) applies.
    /// Control thread; allocates.
    ///
    /// `rate` is the rate the responses were measured at, a whole number of
    /// hertz as the byte format stores it. Every
    /// response has the same length, which becomes the sphere's.
    ///
    /// A set that leaves part of the sphere unmeasured (one hemisphere, say)
    /// has no face there that a ray from the listener crosses: a direction
    /// there takes its nearest measured direction's responses.
    ///
    /// # Errors
    ///
    /// An [`HrtfBinauralError`]: [`FractionalRate`](HrtfBinauralError::FractionalRate)
    /// for a rate that is not a whole number of hertz, [`Rate`](HrtfBinauralError::Rate) or
    /// [`ResponseTooLong`](HrtfBinauralError::ResponseTooLong) for a rate or
    /// length out of range, [`ResponseLength`](HrtfBinauralError::ResponseLength)
    /// for responses of unequal length,
    /// [`Duplicate`](HrtfBinauralError::Duplicate) for two measurements in
    /// one direction, [`NotEnclosing`](HrtfBinauralError::NotEnclosing) for
    /// fewer than four directions or directions in one plane,
    /// [`Inside`](HrtfBinauralError::Inside) for a direction the hull of the
    /// others encloses, and
    /// [`Vertex`](HrtfBinauralError::Vertex) for a non-finite direction or
    /// a response sample out of range.
    pub fn new<S: Sample>(
        rate: SampleRate,
        measurements: &[HrirMeasurement<'_, S>],
    ) -> Result<Self, HrtfBinauralError> {
        // The format stores a `u32`; a rate past it is refused by `check_shape`'s range.
        let hz = rate.get();
        if hz.fract() != 0.0 {
            return Err(HrtfBinauralError::FractionalRate { rate });
        }
        let rate = hz.min(f64::from(u32::MAX)) as u32;
        if measurements.len() < 4 {
            return Err(HrtfBinauralError::NotEnclosing);
        }
        let len = measurements[0].left.samples().len();
        for (vertex, m) in measurements.iter().enumerate() {
            for found in [m.left.samples().len(), m.right.samples().len()] {
                if found != len {
                    return Err(HrtfBinauralError::ResponseLength {
                        vertex,
                        expected: len,
                        found,
                    });
                }
            }
        }
        check_shape(rate, len)?;
        let positions: Vec<[f32; 3]> = measurements
            .iter()
            .map(|m| unit_vector(m.direction))
            .collect();
        let mut responses = Vec::with_capacity(measurements.len() * 2 * len);
        for m in measurements {
            let ears = m.left.samples().iter().chain(m.right.samples());
            responses.extend(ears.map(|&s| s.convert_to::<f32>()));
        }
        // The check on the raw values comes first: a NaN direction would
        // poison the duplicate test and the hull.
        check_values(&positions, &responses, len)?;
        let wide: Vec<[f64; 3]> = positions.iter().map(|p| p.map(f64::from)).collect();
        for (second, p) in wide.iter().enumerate() {
            let twin = wide[..second].iter().position(|q| {
                let d = [p[0] - q[0], p[1] - q[1], p[2] - q[2]];
                d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < DUPLICATE
            });
            if let Some(first) = twin {
                return Err(HrtfBinauralError::Duplicate { first, second });
            }
        }
        let faces = convex_hull(&wide).ok_or(HrtfBinauralError::NotEnclosing)?;
        every_vertex_on(&faces, positions.len())?;
        let sphere = Self {
            rate,
            len,
            positions,
            faces,
            responses,
        };
        sphere.check_faces()?;
        Ok(sphere)
    }

    /// The sphere in the byte format [`from_bytes`](Self::from_bytes) reads.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let words = 5 + self.faces.len() * 3 + self.positions.len() * 3 + self.responses.len();
        let mut bytes = Vec::with_capacity(words * 4);
        bytes.extend_from_slice(b"HRIR");
        for n in [
            self.rate,
            self.len as u32,
            self.positions.len() as u32,
            self.faces.len() as u32 * 3,
        ] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        for &index in self.faces.iter().flatten() {
            bytes.extend_from_slice(&index.to_le_bytes());
        }
        let responses = self.responses.chunks_exact(2 * self.len);
        for (position, response) in self.positions.iter().zip(responses) {
            for value in position.iter().chain(response) {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }

    /// Parses and validates a sphere. Every index, length and value is
    /// checked here, so nothing downstream (and nothing on the audio
    /// thread) has a malformed sphere to trip over.
    ///
    /// # Errors
    ///
    /// An [`HrtfBinauralError`] naming the first thing wrong with the bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, HrtfBinauralError> {
        if bytes.get(..4) != Some(b"HRIR".as_slice()) {
            return Err(HrtfBinauralError::NotASphere);
        }
        let mut cur = Cursor { bytes, at: 4 };
        let rate = cur.u32()?;
        let len = cur.u32()? as usize;
        let vertices = cur.u32()? as usize;
        let indices = cur.u32()? as usize;
        check_shape(rate, len)?;
        if vertices == 0 {
            return Err(HrtfBinauralError::Header("the sphere has no vertices"));
        }
        if !indices.is_multiple_of(3) {
            return Err(HrtfBinauralError::Header(
                "the index count is not a whole number of triangles",
            ));
        }
        // Size the whole file before allocating anything, so a header that
        // claims billions of vertices is refused rather than attempted.
        let needed = vertices
            .checked_mul(len)
            .and_then(|n| n.checked_mul(2))
            .and_then(|n| n.checked_add(vertices.checked_mul(3)?))
            .and_then(|n| n.checked_add(indices))
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_add(20))
            .unwrap_or(usize::MAX);
        if needed > bytes.len() {
            return Err(HrtfBinauralError::Truncated {
                needed,
                found: bytes.len(),
            });
        }
        if needed < bytes.len() {
            return Err(HrtfBinauralError::Trailing {
                size: needed,
                found: bytes.len(),
            });
        }

        let mut faces = Vec::with_capacity(indices / 3);
        for _ in 0..indices / 3 {
            faces.push([cur.u32()?, cur.u32()?, cur.u32()?]);
        }
        let mut positions = Vec::with_capacity(vertices);
        let mut responses = Vec::with_capacity(vertices * len * 2);
        for _ in 0..vertices {
            positions.push([cur.f32()?, cur.f32()?, cur.f32()?]);
            for _ in 0..2 * len {
                responses.push(cur.f32()?);
            }
        }
        let sphere = Self {
            rate,
            len,
            positions,
            faces,
            responses,
        };
        sphere.check_faces()?;
        check_values(&sphere.positions, &sphere.responses, len)?;
        Ok(sphere)
    }

    /// Every face names vertices the sphere has, and at least one is a
    /// triangle a ray from the centre can cross.
    fn check_faces(&self) -> Result<(), HrtfBinauralError> {
        let vertices = self.positions.len();
        for (face, tri) in self.faces.iter().enumerate() {
            if let Some(&vertex) = tri.iter().find(|&&v| v as usize >= vertices) {
                return Err(HrtfBinauralError::FaceIndex {
                    face,
                    vertex,
                    vertices,
                });
            }
        }
        if !self
            .faces
            .iter()
            .any(|&f| Face::new(f, &self.positions).is_some())
        {
            return Err(HrtfBinauralError::NoFace);
        }
        Ok(())
    }
}

/// Each of the `vertices` is a corner of some face: the hull dropped none of them as
/// lying inside the others.
fn every_vertex_on(faces: &[[u32; 3]], vertices: usize) -> Result<(), HrtfBinauralError> {
    let mut on_hull = vec![false; vertices];
    for &v in faces.iter().flatten() {
        on_hull[v as usize] = true;
    }
    match on_hull.iter().position(|&on| !on) {
        Some(vertex) => Err(HrtfBinauralError::Inside { vertex }),
        None => Ok(()),
    }
}

/// The rate and the response length are in range: the checks on a sphere's
/// shape, before its content is read or allocated.
fn check_shape(rate: u32, len: usize) -> Result<(), HrtfBinauralError> {
    if !(MIN_SPHERE_RATE..=MAX_SPHERE_RATE).contains(&rate) {
        return Err(HrtfBinauralError::Rate { rate });
    }
    if len == 0 {
        return Err(HrtfBinauralError::Header("the response length is zero"));
    }
    let max = max_response(rate);
    if len > max {
        return Err(HrtfBinauralError::ResponseTooLong { len, max });
    }
    Ok(())
}

/// Each vertex sits at a usable distance from the origin and holds
/// responses of bounded samples: `responses` is `2 * len` values per
/// position.
fn check_values(
    positions: &[[f32; 3]],
    responses: &[f32],
    len: usize,
) -> Result<(), HrtfBinauralError> {
    for (vertex, (pos, response)) in positions
        .iter()
        .zip(responses.chunks_exact(2 * len))
        .enumerate()
    {
        // NaN fails both comparisons, so a non-finite value is refused too.
        let distance = pos
            .iter()
            .map(|&c| f64::from(c).powi(2))
            .sum::<f64>()
            .sqrt();
        let placed = (MIN_DISTANCE..=MAX_DISTANCE).contains(&distance);
        let bounded = response.iter().all(|s| s.abs() <= MAX_SAMPLE);
        if !placed || !bounded {
            return Err(HrtfBinauralError::Vertex { vertex });
        }
    }
    Ok(())
}

/// The longest response a sphere at `rate` may carry: 50 ms of frames,
/// rounded down. Measured HRIRs last a few milliseconds (IRCAM's 512 taps at
/// 44.1 kHz are 11.6 ms); 50 ms leaves room for a set with its onset delay
/// kept and its tail undecayed. Resampling keeps a response's duration, so
/// a set prepared at any rate holds at most 50 ms of frames there, rounded
/// up: the bound on what `HrtfSet::build` allocates and computes per vertex.
pub(crate) fn max_response(rate: u32) -> usize {
    rate as usize / 20
}

/// The most response samples [`resample_into`] copies tap-major at once:
/// 4 MB of `f32`, so its scratch stays bounded whatever the file's size.
const RESAMPLE_BATCH: usize = 1 << 20;

/// Resamples every response in `responses` (back to back, `len` frames
/// each, a vertex's left then its right) from `from` to `to` Hz by
/// band-limited interpolation, keeping its frequency response, and hands
/// each to `emit(index, response)` as `out_len` frames, in index order.
///
/// Each output tap is the response evaluated between its source taps
/// through a Blackman-windowed sinc, low-passed below the lower of the two
/// Nyquist rates (5 % short of it, so the window's transition band stays
/// under it). The kernel is centred, so nothing is delayed. The taps are then
/// scaled by `from / to`: a response's taps are its continuous form times the
/// sample period, so at twice the rate each tap carries half the weight.
///
/// The responses go in batches holding up to [`RESAMPLE_BATCH`] samples of
/// input and of output, copied tap-major so a
/// weight is applied to the whole batch in one contiguous pass. The kernel's
/// weights depend only on the output tap, so each tap's are evaluated once
/// per batch: the `sin` and `cos` calls cost `out_len` kernel widths per
/// batch, and each response costs only its multiply-adds. A response's sum
/// runs in tap order whatever the batch, so batching does not change a bit.
///
/// The kernel is truncated at the response's ends (taps outside it count as
/// zero), so the first and last few output taps of a response that has not
/// decayed there carry the cut's ripple.
fn resample_into(
    responses: &[f32],
    len: usize,
    from: u32,
    to: u32,
    out_len: usize,
    emit: impl FnMut(usize, &[f32]),
) {
    resample_batched(responses, len, from, to, out_len, RESAMPLE_BATCH, emit);
}

/// [`resample_into`] with the batch size, in samples, a parameter.
fn resample_batched(
    responses: &[f32],
    len: usize,
    from: u32,
    to: u32,
    out_len: usize,
    batch_samples: usize,
    mut emit: impl FnMut(usize, &[f32]),
) {
    /// Kernel half-width, in zero crossings.
    const HALF_WIDTH: f64 = 32.0;
    let step = f64::from(from) / f64::from(to);
    let cutoff = 0.95 * step.recip().min(1.0);
    let sinc = WindowedSinc::new(cutoff, HALF_WIDTH, CosineWindow::BLACKMAN)
        .expect("the cutoff and the crossing count are positive constants");
    let half = sinc.half_width();
    let kernel = |t: f64| sinc.weight(t);
    // Responses per batch: at least one, whatever the size.
    let per_batch = (batch_samples / len.max(out_len)).max(1);
    let mut taps = Vec::new();
    let mut weights = Vec::new();
    let mut sums = Vec::new();
    let mut out = Vec::new();
    for (batch, chunk) in responses.chunks(per_batch * len).enumerate() {
        let count = chunk.len() / len;
        let first = batch * per_batch;
        // Tap-major: tap `m` of every response in the batch at
        // `m * count..(m + 1) * count`.
        taps.clear();
        taps.resize(chunk.len(), 0.0f32);
        for (r, response) in chunk.chunks_exact(len).enumerate() {
            for (m, &s) in response.iter().enumerate() {
                taps[m * count + r] = s;
            }
        }
        sums.resize(count, 0.0f64);
        out.clear();
        out.resize(count * out_len, 0.0f32);
        for n in 0..out_len {
            let t = n as f64 * step;
            let end = ((t + half).floor() as usize).saturating_add(1).min(len);
            let lo = ((t - half).ceil().max(0.0) as usize).min(end);
            weights.clear();
            weights.extend((lo..end).map(|m| kernel(t - m as f64)));
            sums.fill(0.0);
            for (row, &w) in taps[lo * count..end * count]
                .chunks_exact(count)
                .zip(&weights)
            {
                for (sum, &s) in sums.iter_mut().zip(row) {
                    *sum += f64::from(s) * w;
                }
            }
            for (r, &sum) in sums.iter().enumerate() {
                out[r * out_len + n] = (sum * step) as f32;
            }
        }
        for (r, response) in out.chunks_exact(out_len).enumerate() {
            emit(first + r, response);
        }
    }
}

/// One face prepared for lookup: its vertices and the inverse of the matrix
/// whose columns are their positions.
#[derive(Debug)]
struct Face {
    verts: [u32; 3],
    inverse: [[f32; 3]; 3],
}

impl Face {
    /// `None` for a face whose plane passes through the origin: no ray from
    /// the centre crosses it at a single point.
    fn new(verts: [u32; 3], positions: &[[f32; 3]]) -> Option<Self> {
        let [a, b, c] = verts.map(|v| positions[v as usize].map(f64::from));
        // Columns a, b, c; the inverse is the adjugate over the determinant.
        let cross = |u: [f64; 3], v: [f64; 3]| {
            [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ]
        };
        let (bc, ca, ab) = (cross(b, c), cross(c, a), cross(a, b));
        let det = a[0] * bc[0] + a[1] * bc[1] + a[2] * bc[2];
        let scale = [a, b, c]
            .iter()
            .map(|p| p.iter().map(|x| x * x).sum::<f64>().sqrt())
            .product::<f64>();
        // Also refuses a NaN determinant.
        if det.abs().partial_cmp(&(1e-9 * scale)) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        let row = |r: [f64; 3]| r.map(|x| (x / det) as f32);
        Some(Self {
            verts,
            inverse: [row(bc), row(ca), row(ab)],
        })
    }

    /// The barycentric weights of the point where the ray along `dir` meets
    /// this face, or `None` if it misses.
    #[inline]
    fn hit(&self, dir: [f32; 3]) -> Option<[f32; 3]> {
        // `dir = M · coeffs`: the coefficients of `dir` in the face's
        // vertices. The ray hits the face iff all three are non-negative,
        // and the hit point's barycentric weights are them normalized.
        let c = self
            .inverse
            .map(|r| r[0] * dir[0] + r[1] * dir[1] + r[2] * dir[2]);
        let sum = c[0] + c[1] + c[2];
        // Relative slack, so a ray along a shared edge hits both faces.
        let slack = -1e-5 * sum;
        if sum > 0.0 && c.iter().all(|&x| x >= slack) {
            let c = c.map(|x| x.max(0.0));
            let sum = c[0] + c[1] + c[2];
            Some(c.map(|x| x / sum))
        } else {
            None
        }
    }
}

/// Which vertices a direction blends, and by how much.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Weights {
    pub(crate) verts: [u32; 3],
    pub(crate) w: [f32; 3],
}

/// The sphere prepared at one sample rate for the convolver: each vertex's
/// two responses as spectra, the face table, and the FFT plans.
/// Built on the control thread; shared, read-only, by every renderer at that
/// rate.
pub(crate) struct HrtfSet<F: Float + FftNum = f32> {
    /// The rate the responses were resampled to.
    pub(crate) rate: u32,
    /// Response length in frames at `rate`.
    pub(crate) ir_len: usize,
    /// The FFT length: the power of two at or above `step + ir_len - 1`.
    pub(crate) fft_len: usize,
    /// The real-input transform pair at `fft_len`; the renderer's scratch is
    /// [`RealFft::scratch_len`].
    pub(crate) fft: RealFft<F>,
    /// Per vertex, the left ear's `fft_len / 2 + 1` bins then the right's,
    /// each `FFT(response) / fft_len` (the inverse is unnormalized).
    spectra: Vec<Complex<F>>,
    /// Vertex directions, normalized, for the nearest-vertex fallback.
    directions: Vec<[f32; 3]>,
    faces: Vec<Face>,
}

impl<F: Float + FftNum> std::fmt::Debug for HrtfSet<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HrtfSet")
            .field("rate", &self.rate)
            .field("ir_len", &self.ir_len)
            .field("fft_len", &self.fft_len)
            .field("fft", &self.fft)
            .field("faces", &self.faces.len())
            .finish_non_exhaustive()
    }
}

impl<F: Float + FftNum> HrtfSet<F> {
    /// Prepares `sphere` at `rate` for steps of `step` frames. Allocates,
    /// resamples and plans: control thread only.
    ///
    /// The peak memory is the spectra this returns, `vertices × 2 × (fft_len
    /// / 2 + 1)` complex values, plus the resampler's scratch (at most
    /// [`RESAMPLE_BATCH`] samples per buffer): each resampled response is
    /// transformed into its spectrum as it is produced. Time and memory
    /// scale with the file's size times `rate / sphere.rate`; the vertex
    /// count is bounded only by the file's size.
    pub(crate) fn build(sphere: &HrirSphere, rate: u32, step: usize) -> Self {
        let ir_len = if rate == sphere.rate {
            sphere.len
        } else {
            (sphere.len as u64 * u64::from(rate)).div_ceil(u64::from(sphere.rate)) as usize
        };
        let fft_len = (step + ir_len - 1).next_power_of_two();
        let fft = RealFft::new(fft_len);
        let bins = fft.bins();
        let scratch_len = fft.scratch_len();

        let vertices = sphere.positions.len();
        // Response `r` is vertex `r / 2`'s left (even) or right (odd) ear;
        // its spectrum lands at `r * bins`. Each is zero-padded into one
        // reused frame and transformed: no copy of the responses at `rate`
        // outlives the call.
        let zero = Complex::new(F::ZERO, F::ZERO);
        let mut spectra = vec![zero; vertices * 2 * bins];
        let mut frame = vec![F::ZERO; fft_len];
        let mut scratch = vec![zero; scratch_len];
        let norm = <F as Float>::from_f32(1.0 / fft_len as f32);
        let mut transform = |r: usize, response: &[f32]| {
            // The measured responses are `f32`; they enter the render's format here.
            let (head, tail) = frame.split_at_mut(response.len());
            for (d, &x) in head.iter_mut().zip(response) {
                *d = <F as Float>::from_f32(x);
            }
            tail.fill(F::ZERO);
            let out = &mut spectra[r * bins..(r + 1) * bins];
            fft.forward(Mono::new(&frame), out, &mut scratch);
            out.iter_mut().for_each(|s| *s = *s * norm);
        };
        if rate == sphere.rate {
            for (r, response) in sphere.responses.chunks_exact(ir_len).enumerate() {
                transform(r, response);
            }
        } else {
            resample_into(
                &sphere.responses,
                sphere.len,
                sphere.rate,
                rate,
                ir_len,
                &mut transform,
            );
        }

        let directions = sphere
            .positions
            .iter()
            .map(|p| {
                let n = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
                p.map(|c| c / n)
            })
            .collect();
        let faces = sphere
            .faces
            .iter()
            .filter_map(|&f| Face::new(f, &sphere.positions))
            .collect();

        Self {
            rate,
            ir_len,
            fft_len,
            fft,
            spectra,
            directions,
            faces,
        }
    }

    /// Vertex `v`'s spectra: the left ear's, then the right's.
    ///
    /// `v` must be a vertex of the sphere; [`weights`](Self::weights) names
    /// only those.
    #[inline]
    pub(crate) fn spectrum(&self, v: u32) -> (&[Complex<F>], &[Complex<F>]) {
        debug_assert!((v as usize) < self.directions.len());
        let bins = self.fft.bins();
        let at = 2 * v as usize * bins;
        let (left, right) = self.spectra[at..at + 2 * bins].split_at(bins);
        (left, right)
    }

    /// The face the ray along `dir` crosses and the blend of its vertices
    /// there. `hint` is the face the last lookup found, tried first (a source
    /// moves smoothly, so usually it hits again) and updated.
    ///
    /// Never fails: a direction no face covers (a sphere that does not
    /// enclose the listener, or a non-finite direction) takes the nearest
    /// vertex, or vertex 0 if no vertex compares. Allocation-free and
    /// lock-free; the scan is linear in the face count, which only a miss of
    /// the hint pays.
    pub(crate) fn weights(&self, dir: [f32; 3], hint: &mut usize) -> Weights {
        if let Some(face) = self.faces.get(*hint)
            && let Some(w) = face.hit(dir)
        {
            return Weights {
                verts: face.verts,
                w,
            };
        }
        for (i, face) in self.faces.iter().enumerate() {
            if let Some(w) = face.hit(dir) {
                *hint = i;
                return Weights {
                    verts: face.verts,
                    w,
                };
            }
        }
        let mut nearest = 0u32;
        let mut best = f32::NEG_INFINITY;
        for (v, d) in self.directions.iter().enumerate() {
            let dot = d[0] * dir[0] + d[1] * dir[1] + d[2] * dir[2];
            if dot > best {
                best = dot;
                nearest = v as u32;
            }
        }
        Weights {
            verts: [nearest; 3],
            w: [1.0, 0.0, 0.0],
        }
    }
}

#[cfg(test)]
mod tests;
