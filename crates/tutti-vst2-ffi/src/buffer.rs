//! Buffers to safely work with audio samples.

use num_traits::Float;

use std::slice;

/// `AudioBuffer` contains references to the audio buffers for all input and output channels.
#[derive(Debug)]
pub struct AudioBuffer<'a, T: 'a + Float> {
    inputs: &'a [*const T],
    outputs: &'a mut [*mut T],
    samples: usize,
}

impl<'a, T: 'a + Float> AudioBuffer<'a, T> {
    /// Create an `AudioBuffer` from raw pointers.
    /// Only really useful for interacting with the VST API.
    ///
    /// # Safety
    /// `inputs_raw` must point at `input_count` valid channel pointers and
    /// `outputs_raw` at `output_count`, each channel pointer addressing at
    /// least `samples` elements. The output channels must not alias each other
    /// or the inputs, and all of it must outlive `'a`.
    #[inline]
    pub unsafe fn from_raw(
        input_count: usize,
        output_count: usize,
        inputs_raw: *const *const T,
        outputs_raw: *mut *mut T,
        samples: usize,
    ) -> Self {
        Self {
            // SAFETY: the caller upholds this fn's `# Safety` contract: `inputs_raw` points
            // at `input_count` valid channel pointers that outlive `'a`.
            inputs: unsafe { slice::from_raw_parts(inputs_raw, input_count) },
            // SAFETY: as above for `outputs_raw`/`output_count`, which the caller
            // guarantees are not aliased elsewhere.
            outputs: unsafe { slice::from_raw_parts_mut(outputs_raw, output_count) },
            samples,
        }
    }

    /// The number of input channels that this buffer was created for
    #[inline]
    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }

    /// The number of output channels that this buffer was created for
    #[inline]
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// The number of samples in this buffer (same for all channels)
    #[inline]
    pub fn samples(&self) -> usize {
        self.samples
    }

    /// The raw inputs to pass to processReplacing
    #[inline]
    pub fn raw_inputs(&self) -> &[*const T] {
        self.inputs
    }

    /// The raw outputs to pass to processReplacing
    #[inline]
    pub fn raw_outputs(&mut self) -> &mut [*mut T] {
        self.outputs
    }
}
