//! A [`Wave`]'s channels as plain vectors, for a test that indexes them.

use tutti_buffers::{Sample, Wave};

/// The channels of a [`Wave`] as one `Vec` per channel.
pub trait WavePlanes<S> {
    /// A copy of the channels, one `Vec` each.
    fn to_planes(&self) -> Vec<Vec<S>>;
}

impl<S: Sample> WavePlanes<S> for Wave<S> {
    fn to_planes(&self) -> Vec<Vec<S>> {
        self.planes()
            .iter()
            .map(|channel| channel.samples().to_vec())
            .collect()
    }
}
