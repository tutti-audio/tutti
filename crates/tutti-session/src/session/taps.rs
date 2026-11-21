//! What the session lets the host listen to: the heard tap, the master meter, the load meter,
//! and (feature `midi`) the engine's MIDI nodes and the `Session::midi` handle.

use tutti_engine::{BlockCounts, Load};
use tutti_graph::MeterReader;
use tutti_rt::{Counter, Gauge};
use tutti_units::Samples;

use crate::{AppKey, Clock, OutputTapOpen, Session, SessionHeardTapError};

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Opens the tap on **what the engine renders**: every output channel at
    /// the device's width, after the declick fader. It is the engine's f32
    /// output buffer, not the device's bytes: the device layer clamps a long
    /// callback to its `MAX_FRAMES` and converts to the device's sample type
    /// after the engine has written the tap. It is read through the same
    /// [`TapReader`](tutti_graph::TapReader) as a plan tap, and is the default way to record what the
    /// output sounds like. A transport stop fades in it as it does in the
    /// engine's output, so a take has no click there; [`Taps::open_root`](crate::Taps::open_root) is the
    /// pre-fader signal of the graph, aligned by the compiler, and steps at a
    /// stop for a transport-gated source.
    ///
    /// It is written by the engine, on every path that renders through it: a
    /// device callback, [`render`](Self::render), and a block the engine
    /// refuses (silence, whole frames, so a take keeps its timing). An export
    /// forks the graph and renders without the engine, so it never writes to
    /// it. There is one at a time; its ring holds `capacity` frames and what
    /// the reader does not drain in time is dropped and counted
    /// ([`TapReader::take_lost`](tutti_graph::TapReader::take_lost)). It flows from the first
    /// render that begins after this returns, and is closed by
    /// [`close_heard_tap`](Self::close_heard_tap) or, once its reader is
    /// dropped, by the next [`Reclaim`](crate::Phase::Reclaim) phase. The width is
    /// the device's when the session was built: a restart onto a device of
    /// another width counts the tap's blocks lost until it is opened again.
    ///
    /// # Errors
    ///
    /// [`SessionHeardTapError::Detached`] on a session without an engine,
    /// and [`SessionHeardTapError::Tap`]`(Busy)` while another reader is alive. While the
    /// audio thread has not taken the last installs it is not an error: the
    /// call returns [`OutputTapOpen::Retry`] and opens nothing.
    pub fn open_heard_tap(
        &self,
        capacity: Samples,
    ) -> Result<OutputTapOpen<C::Float>, SessionHeardTapError> {
        let (taps, layout) = self
            .output
            .taps
            .as_ref()
            .ok_or(SessionHeardTapError::Detached)?;
        Ok(taps.open(*layout, capacity)?)
    }

    /// Closes the tap [`open_heard_tap`](Self::open_heard_tap) opened, now
    /// rather than at the [`Reclaim`](crate::Phase::Reclaim) phase that follows a
    /// dropped reader. The reader keeps what it was written.
    ///
    /// # Errors
    ///
    /// [`SessionHeardTapError::Detached`] on a session without an engine;
    /// [`SessionHeardTapError::Tap`]`(NotOpen)` when no output tap is open (never opened, or
    /// closed already); nothing changes.
    pub fn close_heard_tap(&self) -> Result<(), SessionHeardTapError> {
        let (taps, _) = self
            .output
            .taps
            .as_ref()
            .ok_or(SessionHeardTapError::Detached)?;
        Ok(taps.close()?)
    }

    /// The master meter: the level of each graph output channel, one
    /// [`Level`](tutti_graph::Level) per channel, with the peak held since
    /// the last read. It is an op of the plan on the outputs (see
    /// [`Meters::master`](crate::Meters::master)), declared and on
    /// from the build, so every render feeds it: a device callback,
    /// [`render`](Self::render), and a detached session's
    /// `render_frame`. It reads before the engine's fold
    /// to the device and its declick, aligned to the outputs' time. A block
    /// the engine refuses (rendered silent because the control side held the
    /// graph for that callback) runs no op, so it does not touch the meter:
    /// the meter holds what it held and counts no frames for it. `None` once
    /// a host has taken it.
    pub fn master_meter(&mut self) -> Option<&mut MeterReader> {
        self.graph.meters().master()
    }

    /// Claims the reader reopened under `id`
    /// ([`crate::SessionEvent::RootReaderReopened`]): held until then.
    ///
    /// # Errors
    ///
    /// [`NotReopened`](crate::NotReopened) when none is held under `id`.
    pub fn take_reopened(
        &mut self,
        id: crate::RootReader,
    ) -> Result<crate::Reopened<C::Float>, crate::NotReopened> {
        self.graph.take_reopened(id)
    }

    /// Hands the master meter's reader to its one consumer; see
    /// [`master_meter`](Self::master_meter) and
    /// [`Meters::take_master`](crate::Meters::take_master).
    pub fn take_master_meter(&mut self) -> Option<MeterReader> {
        self.graph.meters().take_master()
    }

    /// The engine's [`Load`]: each block's render time against its
    /// real-time budget, smoothed. Fed by every block the device callback (or,
    /// offline, [`render`](Self::render)) renders; `None` on a detached
    /// session, which has no engine. Clones read the one cell.
    pub fn load(&self) -> Option<Gauge<Load>> {
        self.output.readings.as_ref().map(|(load, _)| load.clone())
    }

    /// What the engine's blocks amounted to: blocks timed, blocks over
    /// budget, blocks silenced and events dropped, each cumulative (an app
    /// diffs two readings). `None` on a detached session, which has no
    /// engine. Clones read the one cell.
    pub fn blocks(&self) -> Option<Counter<BlockCounts>> {
        self.output
            .readings
            .as_ref()
            .map(|(_, blocks)| blocks.clone())
    }

    /// The engine's MIDI nodes, when the builder inserted them.
    #[cfg(feature = "midi")]
    pub fn midi_nodes(&self) -> Option<&crate::MidiNodes> {
        self.midi.nodes.as_ref()
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// The session's MIDI: live inputs, clips, the hardware input's routes
    /// and the outputs.
    #[cfg(feature = "midi")]
    pub fn midi(&mut self) -> crate::Midi<'_, C::Float> {
        self.midi_and_ids().0
    }

    /// [`midi`](Self::midi) and, beside it, the source of the ids of the
    /// session's resources: the two borrow disjoint fields, so an open that
    /// mints a peer's id holds both.
    #[cfg(feature = "midi")]
    pub(crate) fn midi_and_ids(
        &mut self,
    ) -> (crate::Midi<'_, C::Float>, &mut crate::peer::ResourceIds) {
        let midi = crate::Midi {
            graph: &mut self.graph,
            state: &mut self.midi.state,
            nodes: self.midi.nodes.as_ref(),
            settings: self.transport.settings(),
            meter: self.transport.settings().project_meter().clone(),
            #[cfg(feature = "midi-hardware")]
            io: self.midi.hardware.io.as_mut(),
        };
        (midi, &mut self.resource_ids)
    }
}
