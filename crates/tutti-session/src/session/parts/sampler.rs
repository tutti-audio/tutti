//! [`SamplerPart`] with the `sampler` feature: the disk streamer.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_platform::Executor;
use tutti_rt::Bells;
use tutti_sampler::{DiskStreamer, DiskStreamerConfig, Running};
use tutti_units::SampleRate;

use crate::SessionFloat;

/// The disk streamer.
#[derive(Debug)]
pub(crate) struct SamplerPart<F: SessionFloat> {
    /// Streams the files the session's sampler voices play; `None` when the
    /// clock has no stream.
    pub(crate) streamer: Option<DiskStreamer<Running, F>>,
}

impl<F: SessionFloat> SamplerPart<F> {
    /// The part for a graph at `rate`, with a streamer when `streamer` is set.
    pub(crate) fn new(
        streamer: bool,
        rate: SampleRate,
        executor: &Executor,
        bells: &Bells,
    ) -> crate::Result<Self> {
        // A target with no thread has no streamer: `open_stream` says so by name.
        let streamer = if streamer && crate::keep_pace::THREADS {
            Some(DiskStreamer::start(
                rate,
                DiskStreamerConfig::default(),
                executor,
                bells,
            )?)
        } else {
            None
        };
        Ok(Self { streamer })
    }

    /// The streamer's cycle for this update: it sweeps every ring's retired controls,
    /// which no voice rings for.
    pub(crate) fn wake(&self) {
        if let Some(streamer) = &self.streamer {
            streamer.wake();
        }
    }

    /// Every open stream's ratio (file rate / session rate) is re-derived, so
    /// it plays on at its pitch.
    pub(crate) fn set_sample_rate(&self, rate: SampleRate) {
        if let Some(streamer) = &self.streamer {
            streamer.set_sample_rate(rate);
        }
    }
}
