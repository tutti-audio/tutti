//! What the session lets the host listen to: the heard tap, the main output meter, the load meter,
//! and (feature `midi`) the engine's MIDI nodes and the `Session::midi` handle.

use tutti_engine::{BlockCounts, Load};
use tutti_graph::{MeterReader, TapReader};
use tutti_rt::{Counter, Gauge};
use tutti_units::Samples;

use crate::{AppKey, Clock, Session};

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Opens the tap on **what the engine renders**: every output channel at
    /// the device's width, after the declick fader. It is the engine's f32
    /// output buffer, not the device's bytes: the device layer clamps a long
    /// callback to its `MAX_FRAMES` and converts to the device's sample type
    /// after the engine has written the tap. It is read through the same
    /// [`TapReader`] as a plan tap, and is the default way to record what the
    /// output sounds like. A transport stop fades in it as it does in the
    /// engine's output, so a take has no click there; [`Taps::open_main_output`](crate::Taps::open_main_output) is the
    /// pre-fader signal of the graph, aligned by the compiler, and steps at a
    /// stop for a transport-gated source.
    ///
    /// It is written by the engine, on every path that renders through it: a
    /// device callback, [`render`](Self::render), and a block the engine
    /// refuses (silence, whole frames, so a take keeps its timing). An export
    /// forks the graph and renders without the engine, so it never writes to
    /// it. There is one at a time, and the latest open wins: opening again
    /// ends the earlier reader: it answers `Pull::Ended` once drained. Its ring holds `capacity`
    /// frames and what the reader does not drain in time is dropped and
    /// counted ([`TapReader::take_lost`](tutti_graph::TapReader::take_lost)).
    ///
    /// The reader is returned at once. The tap is the one pending install,
    /// pushed to the audio thread when it has room, and its writes start at the
    /// first render that begins after the push, as a plan tap's start at its
    /// commit. The push is made by this call, or else by the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase or [`render`](Self::render).
    /// The tap is closed by [`close_heard_tap`](Self::close_heard_tap) or, once
    /// its reader is dropped, by the next [`Reclaim`](crate::Phase::Reclaim)
    /// phase. The width is the device's when the session was built: a restart
    /// onto a device of another width counts the tap's blocks lost until it is
    /// opened again.
    pub fn open_heard_tap(&mut self, capacity: Samples) -> TapReader<C::Float> {
        let (taps, layout) = &mut self.output.taps;
        taps.open(*layout, capacity)
    }

    /// Closes the tap [`open_heard_tap`](Self::open_heard_tap) opened, now
    /// rather than at the [`Reclaim`](crate::Phase::Reclaim) phase that follows a
    /// dropped reader. The reader keeps what it was written, then answers `Pull::Ended`. With no tap open
    /// it does nothing. The writes stop from the first render that begins after
    /// the push, as an open's start.
    pub fn close_heard_tap(&mut self) {
        self.output.taps.0.close();
    }

    /// Claims the reader reopened under `id`
    /// ([`crate::SessionEvent::MainOutputReaderReopened`]): held until then.
    ///
    /// # Errors
    ///
    /// [`NotReopened`](crate::NotReopened) when none is held under `id`.
    pub fn take_reopened(
        &mut self,
        id: crate::MainOutputReader,
    ) -> Result<crate::Reopened<C::Float>, crate::NotReopened> {
        self.graph.take_reopened(id)
    }

    /// The main output meter: the level of each graph output channel, one
    /// [`Level`](tutti_graph::Level) per channel, with the peak held since
    /// the last read. It is an op of the plan on the outputs, declared and **on**
    /// from the build, so a host that reads it needs to ask for nothing, and every
    /// render feeds it: a device callback,
    /// [`render`](Self::render). It reads before the engine's fold
    /// to the device and its declick, aligned to the outputs' time. A block
    /// the engine refuses (rendered silent because the control side held the
    /// graph for that callback) runs no op, so it does not touch the meter:
    /// the meter holds what it held and counts no frames for it.
    ///
    /// The reader goes to its one consumer, which owns it from here: dropping
    /// it closes the meter at the next [`Reclaim`](crate::Phase::Reclaim)
    /// phase. `None` once taken.
    ///
    /// It follows the outputs as they are re-wired. **A declared main output of
    /// another width replaces it**
    /// ([`Session::declare_main_output_topology`](crate::Session::declare_main_output_topology)):
    /// the reader not yet taken is swapped for the new one, and one taken is
    /// announced by [`SessionEvent::MainOutputReaderReopened`](crate::SessionEvent::MainOutputReaderReopened),
    /// claimed with [`Session::take_reopened`](crate::Session::take_reopened),
    /// and reads closed ([`is_closed`](MeterReader::is_closed)).
    pub fn take_main_output_meter(&mut self) -> Option<MeterReader> {
        self.graph.meters().take_main_output()
    }

    /// The engine's [`Load`]: each block's render time against its
    /// real-time budget, smoothed. Fed by every block the device callback (or,
    /// offline, [`render`](Self::render)) renders. Clones read the one cell.
    pub fn load(&self) -> Gauge<Load> {
        self.output.readings.0.clone()
    }

    /// What the engine's blocks amounted to: blocks timed, blocks over
    /// budget, blocks silenced and events dropped, each cumulative (an app
    /// diffs two readings). Clones read the one cell.
    pub fn blocks(&self) -> Counter<BlockCounts> {
        self.output.readings.1.clone()
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
            bases: self.transport.bases.clone(),
            executor: self.background.executor().downgrade(),
            #[cfg(feature = "midi-hardware")]
            io: self.midi.hardware.locked(),
            #[cfg(feature = "midi-hardware")]
            link: self.midi.hardware.link(),
        };
        (midi, &mut self.resource_ids)
    }
}
