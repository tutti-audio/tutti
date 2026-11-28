//! [`SamplerPart`] with the `sampler` feature: the disk streamer.

use std::sync::Weak;

use tutti_platform::Executor;
use tutti_rt::{Bells, Reclaim};
use tutti_sampler::{DiskStreamer, DiskStreamerConfig, Running};
use tutti_units::SampleRate;

use crate::SessionFloat;

/// The disk streamer.
#[derive(Debug)]
pub(crate) struct SamplerPart<F: SessionFloat> {
    /// Streams the files the session's sampler voices play; `None` when the
    /// clock has no stream.
    pub(crate) streamer: Option<DiskStreamer<Running, F>>,
    /// The sweep of each stream the session opened, held weakly. A stream
    /// inside a voice has its cells swept twice (here and as the voice's
    /// published entries), which is harmless.
    ///
    /// One entry per live stream handle: each sweep drops the entries whose
    /// handle is gone, so the list is bounded by the streams open and the ones
    /// ended since the last [`Reclaim`](crate::Phase::Reclaim) phase.
    streams: Vec<Weak<dyn Reclaim + Send + Sync>>,
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
        let streamer = if streamer && crate::background::THREADS {
            Some(DiskStreamer::start(
                rate,
                DiskStreamerConfig::default(),
                executor,
                bells,
            )?)
        } else {
            None
        };
        Ok(Self {
            streamer,
            streams: Vec::new(),
        })
    }

    /// Keeps `stream`'s sweep, from the stream handle's
    /// [`reclaimer`](tutti_sampler::StreamHandle::reclaimer).
    pub(crate) fn keep(&mut self, stream: Weak<dyn Reclaim + Send + Sync>) {
        self.streams.push(stream);
    }

    /// Sweeps the open streams' loop and direction cells and forgets the ended
    /// ones; returns how many retired values are still held.
    pub(crate) fn reclaim(&mut self) -> usize {
        let mut pinned = 0;
        self.streams.retain(|stream| {
            let live = stream.upgrade();
            pinned += live.as_ref().map_or(0, |stream| stream.reclaim());
            live.is_some()
        });
        pinned
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// **A stream that ended leaves the list at the next sweep**: the list holds
    /// the streams whose handle lives, and the sweep reaches each of them.
    #[test]
    fn a_sweep_forgets_the_streams_whose_handle_is_gone() {
        let executor = Executor::new();
        let bells = Bells::new(&executor).expect("the bells' tick task");
        let mut part =
            SamplerPart::<f32>::new(false, SampleRate::SR_48K, &executor, &bells).expect("a part");
        let cell = tutti_rt::Publisher::new(0u8);
        let (live, ended) = (
            Arc::new(cell.clone()) as Arc<dyn Reclaim + Send + Sync>,
            Arc::new(tutti_rt::Publisher::new(0u8)) as Arc<dyn Reclaim + Send + Sync>,
        );
        part.keep(Arc::downgrade(&live));
        part.keep(Arc::downgrade(&ended));
        drop(ended);
        cell.publish(1);
        assert_eq!(part.reclaim(), 0, "nothing is pinned");
        assert_eq!(part.streams.len(), 1, "the ended stream left the list");
        drop(live);
        let _ = part.reclaim();
        assert!(part.streams.is_empty(), "and the last with its handle");
    }
}
