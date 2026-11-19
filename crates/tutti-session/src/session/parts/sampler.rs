//! [`SamplerPart`] with the `sampler` feature: the disk streamer and the streams its opens
//! landed.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_platform::ThreadStart;
use tutti_rt::LockExt;
use tutti_rt::Mint;
use tutti_sampler::{DiskStreamerConfig, Open, PendingOpen, Started};
use tutti_units::SampleRate;

use crate::SessionFloat;
use crate::sampler::OpenedStreams;
use crate::{SamplerEvent, SamplerLoad, SessionEvent};

/// The disk streamer, the names of its opens, and what they landed.
#[derive(Debug)]
pub(crate) struct SamplerPart<F: SessionFloat> {
    /// Streams the files the session's sampler voices play; `None` when the
    /// clock has no stream.
    pub(crate) streamer: Option<Started<F>>,
    /// Names the [`SamplerLoad`]s.
    pub(crate) opens: Mint,
    /// What the opens landed, until the host takes it
    /// ([`Session::take_stream`](crate::Session::take_stream)).
    pub(crate) streams: OpenedStreams<F>,
    /// Opens on a stepped streamer that its steps have not answered yet.
    pending: Vec<(SamplerLoad, PendingOpen<F>)>,
}

impl<F: SessionFloat> SamplerPart<F> {
    /// The part for a graph at `rate`, with a streamer when `streamer` is set,
    /// stepped by [`step_until_idle`](Self::step_until_idle) when `thread_start` refuses its thread.
    pub(crate) fn new(streamer: bool, rate: SampleRate, thread_start: ThreadStart) -> Self {
        let streamer =
            streamer.then(|| Started::new(rate, DiskStreamerConfig::default(), thread_start));
        Self {
            streamer,
            opens: Mint::new(),
            streams: Default::default(),
            pending: Vec::new(),
        }
    }

    /// Whether the streamer is stepped: 1 fallback, else 0.
    pub(crate) fn fallbacks(&self) -> u64 {
        u64::from(self.streamer.as_ref().is_some_and(Started::is_stepped))
    }

    /// Runs the stepped streamer's cycles, if it is stepped.
    pub(crate) fn step_until_idle(&mut self) {
        if let Some(streamer) = &mut self.streamer {
            let _ = streamer.step_until_idle();
        }
    }

    /// Opens `path` on a stepped streamer: the open waits in `pending`, and
    /// [`land_opens`](Self::land_opens) answers it once a step has. Nothing
    /// blocks on the reply.
    pub(crate) fn open_stepped(&mut self, id: SamplerLoad, opening: PendingOpen<F>) {
        self.pending.push((id, opening));
    }

    /// Lands each stepped open the streamer has answered, as its event; one it
    /// has not stays pending for a later update.
    pub(crate) fn land_opens(&mut self, out: &mut Vec<SessionEvent>) {
        for (id, opening) in std::mem::take(&mut self.pending) {
            match opening.poll() {
                Open::Pending(opening) => self.pending.push((id, opening)),
                Open::Ready(stream, reader) => {
                    self.streams.lock_recover().insert(id, (stream, reader));
                    out.push(SamplerEvent::Opened { id }.into());
                }
                Open::Failed(error) => out.push(
                    SamplerEvent::OpenFailed {
                        id,
                        error: error.into(),
                    }
                    .into(),
                ),
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_sampler::Placement;

    fn refuse(
        _: &str,
        _: Option<tutti_platform::PriorityRequest>,
        _: Box<dyn FnOnce(&tutti_platform::StopToken) + Send>,
    ) -> std::io::Result<tutti_platform::ThreadHandle<()>> {
        Err(std::io::Error::other("no threads here"))
    }

    /// **A stepped open is polled, never waited on.** Landing before any step
    /// returns at once with the open still pending, and the open lands in the
    /// land that follows a step: the stepped path has no blocking receive, so a
    /// reply the steps have not made yet cannot hang it.
    #[test]
    fn a_stepped_open_stays_pending_until_a_step_answers_it_and_never_blocks() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("tone.wav");
        tutti_fixtures::signal::write_wav_f32(&path, SampleRate::SR_48K, 1, &[0.25; 4_800]);
        let mut part = SamplerPart::<f32>::new(true, SampleRate::SR_48K, refuse);
        let opening = part
            .streamer
            .as_ref()
            .expect("a streamer")
            .open(&path, Placement::HEAD);
        let id = SamplerLoad::mint(&part.opens);
        part.open_stepped(id, opening);

        let mut out = Vec::new();
        part.land_opens(&mut out);
        assert!(out.is_empty(), "no step has answered the open");
        assert_eq!(part.pending.len(), 1, "it stays pending");

        part.step_until_idle();
        part.land_opens(&mut out);
        assert!(
            matches!(&out[..], [SessionEvent::Sampler(SamplerEvent::Opened { id: landed })] if *landed == id),
            "{out:?}"
        );
        assert!(part.pending.is_empty());
        assert!(part.streams.lock_recover().contains_key(&id));
    }
}
