//! A fixed-input FFT resampler over [`RealFft`], generic over the float `F`
//! (`f32` or `f64`) it runs in, `f32` by default.
//!
//! Overlap-add: each `fft_size_in`-frame block is zero-padded to twice its
//! length, transformed, multiplied by a windowed-sinc low-pass, truncated or
//! zero-extended to the output's bins, inverse-transformed at twice
//! `fft_size_out`, and its first half added to the previous block's second
//! half. The block sizes come from `gcd(source, target)`, so one block is a
//! whole number of the rate pair's smallest repeating unit and the ratio is
//! exact, not approximated.
//!
//! Why this is not a dependency. The crate this is adapted from runs
//! rustfft's auto planner, which picks AVX, SSE or NEON at run time and so
//! rounds differently on different CPUs. This runs [`RealFft`], whose inner
//! FFT is rustfft's scalar one, so the output is bit-identical across CPUs on
//! the same OS, libm and rustfft version (the window and the twiddles call the
//! C library's `sin` and `cos`, so not across libms).
//!
//! Licence. The block algorithm, the block-size derivation and the filter
//! design are adapted from rubato 0.16.2 by Henrik Enquist (MIT); the notice is
//! `LICENSE-rubato`, beside this file.

use super::{ResampleError, WindowedSinc};
use crate::CosineWindow;
use crate::FftFloat;
use crate::fft::{Complex, RealFft};
use tutti_buffers::{Mono, MonoMut, Planes, PlanesMut};
use tutti_units::Normalized;

/// One block's worth of resampling: the filter, the plans and the scratch.
#[derive(Debug)]
struct Unit<F: FftFloat> {
    fft_size_in: usize,
    fft_size_out: usize,
    /// The low-pass, already transformed and scaled by `1 / (2 * fft_size_in)`,
    /// which is the inverse transform's normalisation.
    filter_f: Vec<Complex<F>>,
    forward: RealFft<F>,
    inverse: RealFft<F>,
    scratch_fw: Vec<Complex<F>>,
    scratch_inv: Vec<Complex<F>>,
    input_buf: Vec<F>,
    input_f: Vec<Complex<F>>,
    output_f: Vec<Complex<F>>,
    output_buf: Vec<F>,
}

/// The relative cutoff (1.0 is Nyquist) at which a squared Blackman-Harris
/// windowed sinc of `npoints` taps rolls off to -6 dB at the band edge: a cubic
/// fit in `1 / npoints`, good for 32 to 2048 taps. A design figure, in `f64`.
fn cutoff(npoints: usize) -> Normalized {
    let [k1, k2, k3] = [13.745_203, 121.735_33, 5_964.163];
    let n = npoints as f64;
    Normalized::clamped(1.0 / (k1 / n + k2 / (n * n) + k3 / (n * n * n) + 1.0))
}

/// A windowed sinc of `npoints` taps at relative cutoff `f_cutoff`, designed in `f64` summing
/// to `scale` (`1` is unity gain at DC): [`WindowedSinc`] over the squared Blackman-Harris
/// window, whose support is the `npoints` taps and whose peak is tap `npoints / 2`.
fn sinc_design(npoints: usize, f_cutoff: Normalized, scale: f64) -> Vec<f64> {
    let f_cutoff = f_cutoff.get();
    let kernel = WindowedSinc::new(
        f_cutoff,
        npoints as f64 * f_cutoff / 2.0,
        CosineWindow::BLACKMAN_HARRIS_SQUARED,
    )
    .expect("the cutoff and the tap count are positive and finite");
    let mid = (npoints / 2) as f64;
    let mut taps: Vec<f64> = (0..npoints)
        .map(|x| kernel.weight(x as f64 - mid))
        .collect();
    let sum: f64 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t *= scale / sum);
    taps
}

impl<F: FftFloat> Unit<F> {
    fn new(fft_size_in: usize, fft_size_out: usize) -> Self {
        // The filter is cut at the lower of the two Nyquists: the output's
        // when downsampling (anti-alias), the input's when upsampling
        // (anti-image).
        let f_cutoff = if fft_size_in > fft_size_out {
            Normalized::clamped(
                cutoff(fft_size_out).get() * fft_size_out as f64 / fft_size_in as f64,
            )
        } else {
            cutoff(fft_size_in)
        };
        // Scaled by the inverse transform's normalisation, `1 / (2 * fft_size_in)`, in the
        // design, so each tap crosses once.
        let taps = sinc_design(fft_size_in, f_cutoff, 1.0 / (2 * fft_size_in) as f64);
        let mut filter_t = vec![F::ZERO; 2 * fft_size_in];
        for (f, t) in filter_t.iter_mut().zip(taps) {
            *f = F::from_design(t);
        }
        let forward = RealFft::new(2 * fft_size_in);
        let inverse = RealFft::new(2 * fft_size_out);
        let mut scratch_fw = forward.new_scratch();
        let scratch_inv = inverse.new_scratch();
        let mut filter_f = vec![Complex::new(F::ZERO, F::ZERO); fft_size_in + 1];
        forward.forward(Mono::new(&filter_t), &mut filter_f, &mut scratch_fw);
        Self {
            fft_size_in,
            fft_size_out,
            filter_f,
            forward,
            inverse,
            scratch_fw,
            scratch_inv,
            input_buf: vec![F::ZERO; 2 * fft_size_in],
            input_f: vec![Complex::new(F::ZERO, F::ZERO); fft_size_in + 1],
            output_f: vec![Complex::new(F::ZERO, F::ZERO); fft_size_out + 1],
            output_buf: vec![F::ZERO; 2 * fft_size_out],
        }
    }

    /// Resample one block: `wave_in` is `fft_size_in` frames, `wave_out` gets
    /// `fft_size_out`, and `overlap` carries the tail between blocks.
    fn resample_unit(&mut self, wave_in: &[F], wave_out: &mut [F], overlap: &mut [F]) {
        self.input_buf[..self.fft_size_in].copy_from_slice(wave_in);
        self.input_buf[self.fft_size_in..].fill(F::ZERO);
        self.forward.forward(
            Mono::new(&self.input_buf),
            &mut self.input_f,
            &mut self.scratch_fw,
        );

        // The bins both rates share; the rest of the output spectrum is zero.
        let shared = if self.fft_size_in < self.fft_size_out {
            self.fft_size_in + 1
        } else {
            self.fft_size_out
        };
        for ((out, spec), filt) in self
            .output_f
            .iter_mut()
            .zip(&self.input_f)
            .zip(&self.filter_f)
            .take(shared)
        {
            *out = *spec * *filt;
        }
        self.output_f[shared..].fill(Complex::new(F::ZERO, F::ZERO));

        self.inverse.inverse(
            &mut self.output_f,
            MonoMut::new(&mut self.output_buf),
            &mut self.scratch_inv,
        );
        for ((out, &fresh), &carried) in wave_out
            .iter_mut()
            .zip(&self.output_buf)
            .zip(overlap.iter())
            .take(self.fft_size_out)
        {
            *out = fresh + carried;
        }
        overlap.copy_from_slice(&self.output_buf[self.fft_size_out..]);
    }
}

/// The longest block, in frames, either side of one FFT may have.
///
/// A block is the rate pair's smallest repeating unit (`rate / gcd`) times a
/// count, and the FFT is twice that long. The largest unit among real pairs is
/// a coprime one at the top rate: 192000 <-> 44101 is 192000 frames, and 44100
/// <-> 48001 is 48001. The largest preset-sized block, 8000 -> 192000 at a
/// 32768-frame chunk in one piece, is 786432. This is 2^20, so every one of
/// those builds (a 2 M-point FFT, tens of MB) while a pair like 1 -> 192000,
/// which would plan ~98 M points, is refused.
pub(super) const MAX_BLOCK: usize = 1 << 20;

/// A resampler that takes a fixed number of input frames per call and returns
/// a variable number of output frames (whole blocks of `fft_size_out`).
#[derive(Debug)]
pub(crate) struct FftResampler<F: FftFloat = f32> {
    channels: usize,
    chunk_size_in: usize,
    fft_size_in: usize,
    fft_size_out: usize,
    overlaps: Vec<Vec<F>>,
    input_buffers: Vec<Vec<F>>,
    saved_frames: usize,
    unit: Unit<F>,
}

impl<F: FftFloat> FftResampler<F> {
    /// `chunk_size_in` input frames per call, split into about `sub_chunks`
    /// blocks (the real count is whatever makes a block a whole number of the
    /// rate pair's repeating unit).
    ///
    /// # Errors
    /// [`ResampleError`] for a zero rate, channel count, chunk or sub-chunk
    /// count, or a rate pair whose block exceeds the supported size.
    pub(crate) fn new(
        source_hz: usize,
        target_hz: usize,
        chunk_size_in: usize,
        sub_chunks: usize,
        channels: usize,
    ) -> Result<Self, ResampleError> {
        if source_hz == 0 || target_hz == 0 {
            return Err(ResampleError::ZeroRate {
                source_hz,
                target_hz,
            });
        }
        if channels == 0 || chunk_size_in == 0 || sub_chunks == 0 {
            return Err(ResampleError::EmptyShape);
        }
        let gcd = gcd(source_hz, target_hz);
        let min_chunk_in = source_hz / gcd;
        let fft_chunks = (chunk_size_in / sub_chunks).div_ceil(min_chunk_in).max(1);
        let block = |hz: usize| {
            fft_chunks
                .checked_mul(hz / gcd)
                .filter(|&size| size <= MAX_BLOCK)
                .ok_or(ResampleError::BlockTooLarge {
                    source_hz,
                    target_hz,
                })
        };
        let fft_size_out = block(target_hz)?;
        let fft_size_in = block(source_hz)?;
        Ok(Self {
            channels,
            chunk_size_in,
            fft_size_in,
            fft_size_out,
            overlaps: vec![vec![F::ZERO; fft_size_out]; channels],
            input_buffers: vec![vec![F::ZERO; chunk_size_in + fft_size_in]; channels],
            saved_frames: 0,
            unit: Unit::new(fft_size_in, fft_size_out),
        })
    }

    /// Input frames the next call takes, per channel.
    pub(crate) fn input_frames_next(&self) -> usize {
        self.chunk_size_in
    }

    /// The most output frames a call can produce, per channel.
    pub(crate) fn output_frames_max(&self) -> usize {
        self.chunk_size_in.div_ceil(self.fft_size_in) * self.fft_size_out
    }

    /// Output frames at the head that are filter warm-up, not signal.
    pub(crate) fn output_delay(&self) -> usize {
        self.fft_size_out / 2
    }

    /// Resample one chunk (`input_frames_next()` frames per plane) into
    /// `wave_out`, which must hold `output_frames_max()` per plane. Returns the
    /// frames produced per plane.
    ///
    /// # Errors
    /// [`ResampleError::BufferMismatch`] when the planes are too few or too short.
    pub(crate) fn process(
        &mut self,
        wave_in: Planes<'_, F>,
        mut wave_out: PlanesMut<'_, '_, F>,
    ) -> Result<usize, ResampleError> {
        let next_saved = self.saved_frames + self.chunk_size_in;
        let ready = next_saved / self.fft_size_in;
        let needed = ready * self.fft_size_out;
        if wave_in.channels() != self.channels
            || wave_out.channels() != self.channels
            || wave_in.frames().get() < self.chunk_size_in
            || wave_out.frames().get() < needed
        {
            return Err(ResampleError::BufferMismatch {
                channels: self.channels,
                frames_in: self.chunk_size_in,
                frames_out: needed,
            });
        }

        for (buffer, input) in self.input_buffers.iter_mut().zip(wave_in) {
            buffer[self.saved_frames..next_saved]
                .copy_from_slice(&input.samples()[..self.chunk_size_in]);
        }
        self.saved_frames = next_saved;

        for ((buffer, mut out), overlap) in self
            .input_buffers
            .iter()
            .zip(wave_out.iter_mut())
            .zip(self.overlaps.iter_mut())
        {
            for (in_block, out_block) in buffer
                .chunks(self.fft_size_in)
                .take(ready)
                .zip(out.samples_mut().chunks_mut(self.fft_size_out))
            {
                self.unit.resample_unit(in_block, out_block, overlap);
            }
        }

        // Keep the frames short of a whole block for the next call.
        let used = ready * self.fft_size_in;
        if self.saved_frames > used {
            for buffer in &mut self.input_buffers {
                buffer.copy_within(used..self.saved_frames, 0);
            }
        }
        self.saved_frames -= used;
        Ok(needed)
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests;
