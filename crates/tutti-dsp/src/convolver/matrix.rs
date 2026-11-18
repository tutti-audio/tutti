//! A matrix of convolutions: `inputs` channels in, `outputs` channels out, one impulse
//! response per (input, output) path.
//!
//! Output `o` is the sum over inputs `i` of input `i` convolved with the response of path
//! `(i, o)`. Every path is one [`Convolver`], so the partitioning, the FFT and the
//! bit-for-bit independence from the call lengths are the per-channel engine's; this type
//! only routes: it feeds each path its input and sums the paths of an output in ascending
//! input order. A 2 by 2 matrix is true stereo, an N by M matrix is an ambisonic response
//! set, and a diagonal one is the per-channel convolution.

use tutti_buffers::{Mono, MonoMut, Planes, PlanesMut, Sample};
use tutti_types::ChannelLayout;
use tutti_units::Samples;

use super::Convolver;
use crate::FftFloat;

/// Frames a call is cut into: the size of the scratch, fixed at construction.
const CHUNK: usize = 64;

/// Why a [`MatrixConvolver`] could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MatrixConvolverError {
    /// No input or no output channel: a matrix has at least one of each.
    #[error("a matrix convolver needs at least one input and one output channel")]
    NoChannels,
    /// The number of impulse responses is not `inputs * outputs`.
    #[error(
        "a {inputs}-in {outputs}-out matrix convolver needs {want} impulse responses, not {got}"
    )]
    IrCount {
        /// The input channels.
        inputs: usize,
        /// The output channels.
        outputs: usize,
        /// The responses a matrix of that shape has.
        want: usize,
        /// The responses given.
        got: usize,
    },
}

/// A matrix of partitioned FFT convolutions in the float `F`.
///
/// Built once on the control side: every path's spectra, running state and the call
/// scratch are sized at construction, and [`process`](Self::process) allocates nothing.
/// The latency is the partition of the head level (the `block_size` given), the same on
/// every path, and the tail is the longest response's.
#[derive(Clone, Debug)]
pub struct MatrixConvolver<F: FftFloat> {
    inputs: ChannelLayout,
    outputs: ChannelLayout,
    /// The paths, input-major: path `(i, o)` is `paths[i * outputs + o]`.
    paths: Vec<Convolver<F>>,
    /// Each input's frames of the call being rendered.
    heard: Vec<[F; CHUNK]>,
    /// The sum of one output's paths.
    sum: [F; CHUNK],
    /// One path's output.
    path: [F; CHUNK],
}

impl<F: FftFloat> MatrixConvolver<F> {
    /// A matrix of `inputs` by `outputs` channels from `irs`, input-major: the response of
    /// the path from input `i` to output `o` is `irs[i * outputs + o]`. For a 2 by 2
    /// matrix that is `[LL, LR, RL, RR]`, with `out_L = in_L * LL + in_R * RL`.
    ///
    /// `block_size` is rounded up to a power of two; it is the latency.
    ///
    /// # Errors
    ///
    /// [`MatrixConvolverError::NoChannels`] for a width of none, and
    /// [`MatrixConvolverError::IrCount`] when `irs` is not `inputs * outputs` long.
    pub fn new<S: Sample>(
        inputs: ChannelLayout,
        outputs: ChannelLayout,
        irs: &[Mono<'_, S>],
        block_size: Samples,
    ) -> Result<Self, MatrixConvolverError> {
        let (rows, cols) = (usize::from(inputs.count()), usize::from(outputs.count()));
        if rows == 0 || cols == 0 {
            return Err(MatrixConvolverError::NoChannels);
        }
        let want = rows * cols;
        if irs.len() != want {
            return Err(MatrixConvolverError::IrCount {
                inputs: rows,
                outputs: cols,
                want,
                got: irs.len(),
            });
        }
        Ok(Self::assemble(
            inputs,
            outputs,
            irs.iter()
                .map(|ir| Convolver::new(*ir, block_size))
                .collect(),
        ))
    }

    /// A 2 by 2 matrix from `[LL, LR, RL, RR]` ([`new`](Self::new)'s order), which cannot
    /// be the wrong shape.
    #[must_use]
    pub fn two_by_two<S: Sample>(irs: [Mono<'_, S>; 4], block_size: Samples) -> Self {
        Self::assemble(
            ChannelLayout::STEREO,
            ChannelLayout::STEREO,
            irs.iter()
                .map(|ir| Convolver::new(*ir, block_size))
                .collect(),
        )
    }

    /// The matrix over `paths`, input-major, of a shape the caller has checked.
    fn assemble(inputs: ChannelLayout, outputs: ChannelLayout, paths: Vec<Convolver<F>>) -> Self {
        Self {
            heard: vec![[F::ZERO; CHUNK]; usize::from(inputs.count())],
            inputs,
            outputs,
            paths,
            sum: [F::ZERO; CHUNK],
            path: [F::ZERO; CHUNK],
        }
    }

    /// A matrix over these responses' spectra, shared, in the state of a freshly built
    /// one. Allocates the new state.
    #[must_use]
    pub fn fresh(&self) -> Self {
        Self {
            paths: self.paths.iter().map(Convolver::fresh).collect(),
            ..self.clone()
        }
    }

    /// The input channels.
    pub fn inputs(&self) -> ChannelLayout {
        self.inputs
    }

    /// The output channels.
    pub fn outputs(&self) -> ChannelLayout {
        self.outputs
    }

    /// The longest response, in samples.
    pub fn ir_length(&self) -> Samples {
        self.paths
            .iter()
            .map(Convolver::ir_length)
            .max()
            .unwrap_or(Samples::ZERO)
    }

    /// The latency in samples, one head partition on every path.
    pub fn latency(&self) -> Samples {
        self.paths[0].latency()
    }

    /// Convolves a block: output channel `o` is the sum over input channels `i` of input
    /// `i` through path `(i, o)`.
    ///
    /// Input channel `i` of the views feeds row `i`; a row the views are narrower than
    /// reads as silence, and an output channel the matrix is narrower than is not touched.
    /// The frames rendered are the fewer of the two views'. The render is the same bits
    /// for any split of the stream into calls. No allocation.
    pub fn process(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.frames().get().min(output.frames().get());
        let (rows, cols) = (
            usize::from(self.inputs.count()),
            usize::from(self.outputs.count()),
        );
        let mut at = 0;
        while at < frames {
            let n = CHUNK.min(frames - at);
            for (i, row) in self.heard.iter_mut().enumerate() {
                match input.get_plane(i) {
                    Some(src) => row[..n].copy_from_slice(&src.samples()[at..at + n]),
                    None => row[..n].fill(F::ZERO),
                }
            }
            for o in 0..cols {
                self.sum[..n].fill(F::ZERO);
                for i in 0..rows {
                    self.paths[i * cols + o].process_block(
                        Mono::new(&self.heard[i][..n]),
                        MonoMut::new(&mut self.path[..n]),
                    );
                    for (s, p) in self.sum[..n].iter_mut().zip(&self.path[..n]) {
                        *s += *p;
                    }
                }
                if let Some(mut plane) = output.get_plane_mut(o) {
                    plane.samples_mut()[at..at + n].copy_from_slice(&self.sum[..n]);
                }
            }
            at += n;
        }
    }

    /// Zeros every path's running state, keeping the responses.
    pub fn reset(&mut self) {
        for path in &mut self.paths {
            path.reset();
        }
    }
}

#[cfg(test)]
mod tests;
