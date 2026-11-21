//! The orthonormal DCT-II over a short vector, the transform that turns log mel energies
//! into cepstral coefficients.
//!
//! `c_k = s_k * sum over n of x_n cos(pi k (2n + 1) / 2N)` for `n` in `0..N`, with
//! `s_0 = sqrt(1 / N)` and `s_k = sqrt(2 / N)` (Ahmed, Natarajan and Rao, 1974). The scaling
//! makes the `N x N` matrix orthogonal, so the transform preserves the vector's norm and
//! its inverse is its transpose. It is applied directly in `O(N * K)` from a cosine table
//! built once, which suits the 20 to 128 bands of a mel bank.

use core::num::NonZeroUsize;

use tutti_buffers::{Float, Sample};

/// More coefficients asked of a DCT-II than it has points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{outputs} coefficients asked of a transform of {inputs} points")]
pub struct TooManyOutputs {
    /// The coefficients asked for.
    pub outputs: usize,
    /// The points of the transform, the most it can give.
    pub inputs: usize,
}

/// A DCT-II of `inputs` points that keeps the first `outputs` coefficients.
#[derive(Debug, Clone, PartialEq)]
pub struct Dct2 {
    inputs: usize,
    outputs: usize,
    table: Vec<f64>,
}

impl Dct2 {
    /// The transform of `inputs` points keeping `outputs` coefficients.
    ///
    /// # Errors
    ///
    /// [`TooManyOutputs`] when `outputs` is more than `inputs`.
    pub fn new(inputs: NonZeroUsize, outputs: NonZeroUsize) -> Result<Self, TooManyOutputs> {
        let n = inputs.get();
        let k_count = outputs.get();
        if k_count > n {
            return Err(TooManyOutputs {
                outputs: k_count,
                inputs: n,
            });
        }
        let table = (0..k_count)
            .flat_map(|k| {
                let s = if k == 0 {
                    libm::sqrt(1.0 / n as f64)
                } else {
                    libm::sqrt(2.0 / n as f64)
                };
                (0..n).map(move |i| {
                    s * libm::cos(core::f64::consts::PI * (k * (2 * i + 1)) as f64 / (2 * n) as f64)
                })
            })
            .collect();
        Ok(Self {
            inputs: n,
            outputs: k_count,
            table,
        })
    }

    /// The points transformed.
    #[must_use]
    pub fn inputs(&self) -> usize {
        self.inputs
    }

    /// The coefficients kept.
    #[must_use]
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// Writes the first coefficients of `input` into `output`: as many as `output` has room
    /// for, up to [`outputs`](Self::outputs), reading as many points as `input` holds up to
    /// [`inputs`](Self::inputs) (a missing point is zero). Each is summed in `f64` and
    /// narrowed once. Allocates nothing.
    pub fn process<I: Float, O: Float>(&self, input: &[I], output: &mut [O]) {
        for (out, row) in output.iter_mut().zip(self.table.chunks_exact(self.inputs)) {
            let sum: f64 = row
                .iter()
                .zip(input)
                .map(|(c, &x)| c * x.convert_to::<f64>())
                .sum();
            *out = sum.convert_to::<O>();
        }
    }
}

#[cfg(test)]
mod tests;
