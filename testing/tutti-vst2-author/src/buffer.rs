//! Audio buffer views for a plugin's `process`: [`AudioBufferExt`] splits and zips the
//! host's [`AudioBuffer`] into safe channel slices.

use std::ops::{Index, IndexMut};
use std::slice;

use num_traits::Float;

pub use tutti_vst2_ffi::buffer::AudioBuffer;

/// The channel views of an [`AudioBuffer`].
pub trait AudioBufferExt<'a, T: 'a + Float> {
    /// Split this buffer into separate inputs and outputs.
    fn split<'b>(&'b mut self) -> (Inputs<'b, T>, Outputs<'b, T>)
    where
        'a: 'b;

    /// Create an iterator over pairs of input buffers and output buffers.
    fn zip<'b>(&'b mut self) -> AudioBufferIterator<'a, 'b, T>;
}

impl<'a, T: 'a + Float> AudioBufferExt<'a, T> for AudioBuffer<'a, T> {
    #[inline]
    fn split<'b>(&'b mut self) -> (Inputs<'b, T>, Outputs<'b, T>)
    where
        'a: 'b,
    {
        let samples = self.samples();
        // The outputs are read first: `raw_outputs` borrows `self` mutably for `'b`, and
        // the inputs slice is a distinct allocation of pointers.
        let inputs: *const [*const T] = self.raw_inputs();
        let outputs: &'b mut [*mut T] = self.raw_outputs();
        (
            Inputs {
                // SAFETY: `inputs` addresses the buffer's input pointer table, which lives
                // for `'a: 'b` and which nothing writes.
                bufs: unsafe { &*inputs },
                samples,
            },
            Outputs {
                bufs: outputs,
                samples,
            },
        )
    }

    #[inline]
    fn zip<'b>(&'b mut self) -> AudioBufferIterator<'a, 'b, T> {
        AudioBufferIterator {
            audio_buffer: self,
            index: 0,
        }
    }
}

/// Iterator over pairs of buffers of input channels and output channels.
#[derive(Debug)]
pub struct AudioBufferIterator<'a, 'b, T>
where
    T: 'a + Float,
    'a: 'b,
{
    audio_buffer: &'b mut AudioBuffer<'a, T>,
    index: usize,
}

impl<'a, 'b, T> Iterator for AudioBufferIterator<'a, 'b, T>
where
    T: 'b + Float,
{
    type Item = (&'b [T], &'b mut [T]);

    fn next(&mut self) -> Option<Self::Item> {
        let samples = self.audio_buffer.samples();
        let index = self.index;
        let input = *self.audio_buffer.raw_inputs().get(index)?;
        let output = *self.audio_buffer.raw_outputs().get(index)?;
        self.index += 1;
        // SAFETY: `AudioBuffer`'s constructor contract (`from_raw`) says each channel
        // pointer addresses at least `samples` live elements; the index is bounds-checked
        // by the `get`s above.
        let input = unsafe { slice::from_raw_parts(input, samples) };
        // SAFETY: as for `input`; output channels do not alias each other or the inputs
        // (constructor contract), and the iterator yields each index once.
        let output = unsafe { slice::from_raw_parts_mut(output, samples) };
        Some((input, output))
    }
}

/// Wrapper type to access the buffers for the input channels of an `AudioBuffer` in a safe way.
/// Behaves like a slice.
#[derive(Copy, Clone, Debug)]
pub struct Inputs<'a, T: 'a> {
    bufs: &'a [*const T],
    samples: usize,
}

impl<'a, T> Inputs<'a, T> {
    /// Number of channels
    pub fn len(&self) -> usize {
        self.bufs.len()
    }

    /// Returns true if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Access channel at the given index
    pub fn get(&self, i: usize) -> &'a [T] {
        // SAFETY: `i` is bounds-checked by the indexing; the `AudioBuffer` constructor
        // contract says each channel pointer addresses `samples` live elements for `'a`.
        unsafe { slice::from_raw_parts(self.bufs[i], self.samples) }
    }

    /// Split borrowing at the given index, like for slices
    pub fn split_at(&self, i: usize) -> (Inputs<'a, T>, Inputs<'a, T>) {
        let (l, r) = self.bufs.split_at(i);
        (
            Inputs {
                bufs: l,
                samples: self.samples,
            },
            Inputs {
                bufs: r,
                samples: self.samples,
            },
        )
    }
}

impl<'a, T> Index<usize> for Inputs<'a, T> {
    type Output = [T];

    fn index(&self, i: usize) -> &Self::Output {
        self.get(i)
    }
}

/// Iterator over buffers for input channels of an `AudioBuffer`.
#[derive(Debug)]
pub struct InputIterator<'a, T: 'a> {
    data: Inputs<'a, T>,
    i: usize,
}

impl<'a, T> Iterator for InputIterator<'a, T> {
    type Item = &'a [T];

    fn next(&mut self) -> Option<Self::Item> {
        if self.i < self.data.len() {
            let val = self.data.get(self.i);
            self.i += 1;
            Some(val)
        } else {
            None
        }
    }
}

impl<'a, T: Sized> IntoIterator for Inputs<'a, T> {
    type Item = &'a [T];
    type IntoIter = InputIterator<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        InputIterator { data: self, i: 0 }
    }
}

/// Wrapper type to access the buffers for the output channels of an `AudioBuffer` in a safe way.
/// Behaves like a slice.
#[derive(Debug)]
pub struct Outputs<'a, T: 'a> {
    bufs: &'a [*mut T],
    samples: usize,
}

impl<'a, T> Outputs<'a, T> {
    /// Number of channels
    pub fn len(&self) -> usize {
        self.bufs.len()
    }

    /// Returns true if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Access channel at the given index
    pub fn get(&self, i: usize) -> &'a [T] {
        // SAFETY: `i` is bounds-checked by the indexing; the `AudioBuffer` constructor
        // contract says each channel pointer addresses `samples` live elements for `'a`.
        unsafe { slice::from_raw_parts(self.bufs[i], self.samples) }
    }

    /// Mutably access channel at the given index.
    ///
    /// The slice borrows `self`, so two live slices of one channel cannot exist:
    ///
    /// ```compile_fail
    /// # use tutti_vst2_ffi::host::HostBuffer;
    /// let mut host_buffer: HostBuffer<f32> = HostBuffer::new(0, 1);
    /// let inputs: Vec<Vec<f32>> = vec![];
    /// let mut outputs = vec![vec![0.0f32; 4]];
    /// let mut buffer = host_buffer.bind(&inputs, &mut outputs);
    /// let (_, mut out) = buffer.split();
    /// let a = out.get_mut(0);
    /// let b = out.get_mut(0);
    /// a[0] = b[0];
    /// ```
    pub fn get_mut(&mut self, i: usize) -> &mut [T] {
        // SAFETY: bounds and length are as in `get`. The output channels do not alias each
        // other (the `AudioBuffer` constructor contract), and the returned slice borrows
        // `&mut self`, so a second call for the same `i` cannot overlap it.
        unsafe { slice::from_raw_parts_mut(self.bufs[i], self.samples) }
    }

    /// Split borrowing at the given index, like for slices
    pub fn split_at_mut(self, i: usize) -> (Outputs<'a, T>, Outputs<'a, T>) {
        let (l, r) = self.bufs.split_at(i);
        (
            Outputs {
                bufs: l,
                samples: self.samples,
            },
            Outputs {
                bufs: r,
                samples: self.samples,
            },
        )
    }
}

impl<'a, T> Index<usize> for Outputs<'a, T> {
    type Output = [T];

    fn index(&self, i: usize) -> &Self::Output {
        self.get(i)
    }
}

impl<'a, T> IndexMut<usize> for Outputs<'a, T> {
    fn index_mut(&mut self, i: usize) -> &mut Self::Output {
        self.get_mut(i)
    }
}

/// Iterator over buffers for output channels of an `AudioBuffer`.
#[derive(Debug)]
pub struct OutputIterator<'a, 'b, T>
where
    T: 'a,
    'a: 'b,
{
    data: &'b mut Outputs<'a, T>,
    i: usize,
}

impl<'a, 'b, T> Iterator for OutputIterator<'a, 'b, T>
where
    T: 'b,
{
    type Item = &'b mut [T];

    fn next(&mut self) -> Option<Self::Item> {
        if self.i < self.data.len() {
            // SAFETY: `i` is in bounds (checked above). `data` is borrowed mutably for `'b`
            // and `i` only grows, so each channel is handed out once per iterator and no
            // two items overlap; the output channels do not alias each other (the
            // `AudioBuffer` constructor contract), each addressing `samples` elements.
            let val =
                unsafe { slice::from_raw_parts_mut(self.data.bufs[self.i], self.data.samples) };
            self.i += 1;
            Some(val)
        } else {
            None
        }
    }
}

impl<'a, 'b, T: Sized> IntoIterator for &'b mut Outputs<'a, T> {
    type Item = &'b mut [T];
    type IntoIter = OutputIterator<'a, 'b, T>;

    fn into_iter(self) -> Self::IntoIter {
        OutputIterator { data: self, i: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Size of buffers used in tests.
    const SIZE: usize = 1024;

    /// Test that creating and zipping buffers works.
    ///
    /// This test creates a channel for 2 inputs and 2 outputs.
    /// The input channels are simply values
    /// from 0 to `SIZE-1` (e.g. [0, 1, 2, 3, 4, .. , SIZE - 1])
    /// and the output channels are just 0.
    /// This test assures that when the buffers are zipped together,
    /// the input values do not change.
    #[test]
    fn buffer_zip() {
        let in1: Vec<f32> = (0..SIZE).map(|x| x as f32).collect();
        let in2 = in1.clone();

        let mut out1 = vec![0.0; SIZE];
        let mut out2 = out1.clone();

        let inputs = [in1.as_ptr(), in2.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(2, 2, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        for (input, output) in buffer.zip() {
            input
                .iter()
                .zip(output.iter_mut())
                .fold(0, |acc, (input, output)| {
                    assert_eq!(*input, acc as f32);
                    assert_eq!(*output, 0.0);
                    acc + 1
                });
        }
    }

    // Test that the `zip()` method returns an iterator that gives `n` elements
    // where n is the number of inputs when this is lower than the number of outputs.
    #[test]
    fn buffer_zip_fewer_inputs_than_outputs() {
        let in1 = vec![1.0; SIZE];
        let in2 = vec![2.0; SIZE];

        let mut out1 = vec![3.0; SIZE];
        let mut out2 = vec![4.0; SIZE];
        let mut out3 = vec![5.0; SIZE];

        let inputs = [in1.as_ptr(), in2.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr(), out3.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(2, 3, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        let mut iter = buffer.zip();
        if let Some((observed_in1, observed_out1)) = iter.next() {
            assert_eq!(1.0, observed_in1[0]);
            assert_eq!(3.0, observed_out1[0]);
        } else {
            unreachable!();
        }

        if let Some((observed_in2, observed_out2)) = iter.next() {
            assert_eq!(2.0, observed_in2[0]);
            assert_eq!(4.0, observed_out2[0]);
        } else {
            unreachable!();
        }

        assert_eq!(None, iter.next());
    }

    // Test that the `zip()` method returns an iterator that gives `n` elements
    // where n is the number of outputs when this is lower than the number of inputs.
    #[test]
    fn buffer_zip_more_inputs_than_outputs() {
        let in1 = vec![1.0; SIZE];
        let in2 = vec![2.0; SIZE];
        let in3 = vec![3.0; SIZE];

        let mut out1 = vec![4.0; SIZE];
        let mut out2 = vec![5.0; SIZE];

        let inputs = [in1.as_ptr(), in2.as_ptr(), in3.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(3, 2, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        let mut iter = buffer.zip();

        if let Some((observed_in1, observed_out1)) = iter.next() {
            assert_eq!(1.0, observed_in1[0]);
            assert_eq!(4.0, observed_out1[0]);
        } else {
            unreachable!();
        }

        if let Some((observed_in2, observed_out2)) = iter.next() {
            assert_eq!(2.0, observed_in2[0]);
            assert_eq!(5.0, observed_out2[0]);
        } else {
            unreachable!();
        }

        assert_eq!(None, iter.next());
    }

    /// Test that creating buffers from raw pointers works.
    #[test]
    fn from_raw() {
        let in1: Vec<f32> = (0..SIZE).map(|x| x as f32).collect();
        let in2 = in1.clone();

        let mut out1 = vec![0.0; SIZE];
        let mut out2 = out1.clone();

        let inputs = [in1.as_ptr(), in2.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(2, 2, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        for (input, output) in buffer.zip() {
            input
                .iter()
                .zip(output.iter_mut())
                .fold(0, |acc, (input, output)| {
                    assert_eq!(*input, acc as f32);
                    assert_eq!(*output, 0.0);
                    acc + 1
                });
        }
    }
}
