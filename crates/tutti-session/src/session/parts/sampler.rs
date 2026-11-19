//! [`SamplerPart`] with the `sampler` feature: the disk streamer and the streams its opens
//! landed.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_platform::ThreadStart;
use tutti_rt::Mint;
use tutti_sampler::{DiskStreamerConfig, Started};
use tutti_units::SampleRate;

use crate::SessionFloat;
use crate::sampler::OpenedStreams;

/// The disk streamer, the names of its opens, and what they landed.
#[derive(Debug)]
pub(crate) struct SamplerPart<F: SessionFloat> {
    /// Streams the files the session's sampler voices play; `None` when the
    /// clock has no stream.
    pub(crate) streamer: Option<Started<F>>,
    /// Names the [`SamplerLoad`](crate::SamplerLoad)s.
    pub(crate) opens: Mint,
    /// What the opens landed, until the host takes it
    /// ([`Session::take_stream`](crate::Session::take_stream)).
    pub(crate) streams: OpenedStreams<F>,
}

impl<F: SessionFloat> SamplerPart<F> {
    /// The part for a graph at `rate`, with a streamer when `streamer` is set,
    /// stepped by [`settle`](Self::settle) when `thread_start` refuses its thread.
    pub(crate) fn new(streamer: bool, rate: SampleRate, thread_start: ThreadStart) -> Self {
        let streamer =
            streamer.then(|| Started::new(rate, DiskStreamerConfig::default(), thread_start));
        Self {
            streamer,
            opens: Mint::new(),
            streams: Default::default(),
        }
    }

    /// Whether the streamer is stepped: 1 fallback, else 0.
    pub(crate) fn fallbacks(&self) -> u64 {
        u64::from(self.streamer.as_ref().is_some_and(Started::is_stepped))
    }

    /// Runs the stepped streamer's cycles, if it is stepped.
    pub(crate) fn settle(&mut self) {
        if let Some(streamer) = &mut self.streamer {
            streamer.settle();
        }
    }

    /// Every open stream's ratio (file rate / session rate) is re-derived, so
    /// it plays on at its pitch.
    pub(crate) fn set_sample_rate(&self, rate: SampleRate) {
        match &self.streamer {
            Some(Started::Thread(streamer)) => streamer.set_sample_rate(rate),
            Some(Started::Stepped(streamer)) => streamer.set_sample_rate(rate),
            None => {}
        }
    }
}
