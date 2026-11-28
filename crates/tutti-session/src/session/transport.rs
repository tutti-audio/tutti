//! The session's transport and clock: the rate, the tempo map and the playhead, play, stop,
//! locate, loop and scheduling calls, the plugin-delay compensation table, and the metronome.

use std::sync::Arc;

use tutti_graph::NodeKey;
use tutti_platform::monotonic_ns;
use tutti_timeline::{FadeOut, MotionCommand, TimedError};
use tutti_units::{At, Beat, SampleRate, Samples};

use crate::{AppKey, Clock, Playhead, PrerollTable, Session, SetRateError, Stepped};

impl<K: AppKey, C: Stepped> Session<K, C> {
    /// Re-prepares the graph for `rate`, and the rest of the session with it:
    /// the transport converts frame-timed commands in the new rate's frames,
    /// MIDI's outputs and every open sampler stream follow, and the figures of
    /// the plan sent are published. With the executor local, both halves of
    /// the re-prepare run here, now; with it taken, the second half lands on
    /// a later update, which publishes the resumed plan's figures at its
    /// commit (until then they are the old plan's). A session on a device
    /// re-rates with its restart.
    ///
    /// # Errors
    ///
    /// [`SetRateError::RateUnsupported`] naming a node that does not run at
    /// `rate` ([`Shape::with_rates`](tutti_graph::Shape::with_rates)), or
    /// [`SetRateError::Commit`] when the graph would not re-prepare; the
    /// session keeps its rate.
    pub fn set_sample_rate(&mut self, rate: SampleRate) -> Result<(), SetRateError> {
        self.graph.set_sample_rate(rate)?;
        self.transport.set_sample_rate(rate);
        self.follow_rerate(rate, |_| {});
        Ok(())
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// What the session follows once the graph has re-prepared for `rate` and
    /// the transport was moved to it: MIDI's outputs reopen at it and commit,
    /// then `resume` runs (a restart's reattempt and the executor's adoption of
    /// the plan), then the figures of the plan sent are published and every
    /// open sampler stream's ratio is re-derived. The one routine behind
    /// [`set_sample_rate`](Session::set_sample_rate) and a device restart.
    pub(crate) fn follow_rerate(&mut self, rate: SampleRate, resume: impl FnOnce(&mut Self)) {
        // The outputs' taps read frames of one rate: reopened at the new one,
        // committed with the rest.
        if self
            .midi
            .follow_rate(&mut self.graph, tutti_platform::Instant::now())
        {
            self.commit();
        }
        resume(self);
        // The figures the first block runs at, published before it.
        self.graph.publish_sent();
        // Every open stream's ratio (file rate / session rate) is
        // re-derived, so it plays on at its pitch.
        self.sampler.set_sample_rate(rate);
    }

    /// The rate the graph is prepared at; it follows the device's.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.transport.sample_rate()
    }

    /// The playhead, in one read: its beat, the graph frame, the transport
    /// time, whether it is rolling, and the OS clock reading they were taken
    /// at ([`Playhead`]). The audio thread writes it once per render call
    /// through a sequence lock, so the fields are one instant, never a beat
    /// of one call beside the play state of another.
    ///
    /// It moves in block steps; [`beat_now`](Self::beat_now) (or
    /// [`Playhead::beat_now`] on a snapshot already held) is the beat
    /// extrapolated to the moment a UI draws. A session with no engine
    /// rendering (detached, or before the first block) reads
    /// [`Playhead::default`]: stopped at the origin, with no clock reading
    /// ([`Playhead::at`] is `None`). An offline bounce renders a fork on
    /// its own timeline and never moves this one.
    #[must_use]
    pub fn playhead(&self) -> Playhead {
        self.transport.handle.playhead()
    }

    /// The playhead's beat now, extrapolated from the last block by
    /// [`Playhead::beat_now`] on [`monotonic_ns`], the clock the
    /// block was stamped on.
    ///
    /// On a session dated by [`BlockDates::Frames`](crate::BlockDates::Frames)
    /// the blocks are not on that clock: the extrapolation is measured from
    /// the block's frame time, so it reads the block's beat or at most one
    /// block past it.
    #[must_use]
    pub fn beat_now(&self) -> Beat {
        self.playhead().beat_now(monotonic_ns())
    }

    /// Starts the transport rolling from where it stands, at the next block.
    /// A no-op while rolling. [`TransportHandle::play`](crate::TransportHandle::play) is the one
    /// implementation.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the transport's command queue is full; the
    /// event is handed back in it.
    pub fn play(&self) -> Result<(), TimedError<MotionCommand>> {
        self.transport.handle.play()
    }

    /// Stops the transport where it stands, at the next block, after the
    /// [`FadeOut`] it names.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn stop(&self, fade: FadeOut) -> Result<(), TimedError<MotionCommand>> {
        self.transport.handle.stop(fade)
    }

    /// Jumps to `beat` at the next block, keeping whatever the transport was
    /// doing (a locate while rolling keeps rolling), with a declick fade.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn locate(&self, beat: Beat) -> Result<(), TimedError<MotionCommand>> {
        self.transport.handle.locate(beat)
    }

    /// Sends a motion `event` to take effect at `at`; see
    /// [`TransportHandle::schedule`](crate::TransportHandle::schedule), the one implementation.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the timed-command list is full, and
    /// [`TimedError::BeatWhileStopped`] for a command that starts playback at
    /// a beat while the transport is stopped.
    pub fn schedule(
        &self,
        at: impl Into<At>,
        event: MotionCommand,
    ) -> Result<(), TimedError<MotionCommand>> {
        self.transport.handle.schedule(at, event)
    }

    /// A [`TransportHandle`](crate::TransportHandle) on this session's transport: `Send`, so another
    /// thread drives play, stop, locate and schedule.
    #[must_use]
    pub fn transport_handle(&self) -> crate::TransportHandle {
        self.transport.handle.clone()
    }

    /// The per-channel compensation of the plan sent last, by output channel:
    /// what the compiler delays each output by, for a host that displays it.
    /// Republished with every plan; empty until the first. A node in the graph
    /// (a disk voice included) is aligned by the compiler, not by this table.
    ///
    /// Shared, not copied: a clone of the `Arc` reads every later plan's table. Read a channel with
    /// [`PrerollTable::preroll`], `None` past the table's end. No audio
    /// thread reads it, so it is a plain lock, not a published cell.
    pub fn compensation(&self) -> &Arc<PrerollTable> {
        self.graph.compensation()
    }

    /// The total latency of the plan sent last: what a DAW displays.
    pub fn latency(&self) -> Samples {
        self.graph.latency()
    }

    /// The metronome's node (unwired: the host declares where it lands),
    /// when the builder inserted one.
    pub fn click_node(&self) -> Option<NodeKey> {
        self.transport.click
    }

    /// The session's MPE mode as a target, written with [`set`](Self::set)
    /// and read with [`get`](Self::get).
    #[cfg(feature = "midi")]
    #[must_use]
    pub const fn mpe(&self) -> crate::MpeModeTarget {
        crate::MpeModeTarget::new()
    }

    /// The metronome's targets: its level and when it sounds, written with
    /// [`set`](Self::set) and read with [`get`](Self::get).
    #[must_use]
    pub const fn click(&self) -> crate::ClickTargets {
        crate::ClickTargets::new()
    }
}
