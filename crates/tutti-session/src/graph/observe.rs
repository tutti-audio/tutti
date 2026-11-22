//! The two facets of observing a [`Graph`]: [`Taps`] (audio and event copies
//! out of it) and [`Meters`] (level readers on it).
//!
//! A facet borrows the graph for the call (`g.taps().open_ports(..)`), so the
//! graph's own type keeps adding, removing and wiring nodes. Both edit the
//! graph's plan (a tap or a meter is an op in it), so both mark it edited and
//! the next commit compiles them.

use tutti_graph::{
    CloseError, EventTapId, EventTapOptions, EventTapReader, EventTapSource, MeterId, MeterReader,
    MeterSpec, MissingSource, TapAlign, TapId, TapOptions, TapReader,
};
use tutti_units::Samples;

use tutti_buffers::Float;

use super::Graph;
#[cfg(feature = "analyze")]
use crate::{Analyzer, AnalyzerError, AudioContext, AudioInput};
use crate::{EventTapPort, OutPort};

/// What a reader of the main output is named by: the id of the tap or of the meter.
/// The reader a reopen opens takes the id of the one it supersedes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum MainOutputReader {
    /// A tap on the main output ([`Taps::open_main_output`]).
    Tap(TapId),
    /// The master meter.
    Meter(MeterId),
}

/// A reader the session opened again because the main output's width changed, claimed
/// with [`Session::take_reopened`](crate::Session::take_reopened) after
/// [`SessionEvent::MainOutputReaderReopened`](crate::SessionEvent::MainOutputReaderReopened).
///
/// The reader it supersedes reads closed: a tap's old reader reads the frames
/// its rings hold and then answers [`Pull::Ended`](tutti_buffers::Pull::Ended);
/// a meter's old reader reads [`is_closed`](MeterReader::is_closed) once the
/// plan that ran it is retired.
#[derive(Debug)]
#[non_exhaustive]
pub enum Reopened<F: Float = f32> {
    /// A tap on the main output, as wide as the main output.
    Tap(TapReader<F>),
    /// The master meter, measuring the main output as wide as it is. It is on.
    Meter(MeterReader),
}

/// No reopened reader is held under the id: it was never reopened, or was
/// claimed already.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no reopened reader is held under {0:?}")]
pub struct NotReopened(pub MainOutputReader);

/// A reopened reader the graph holds, and whether its event was sent.
#[derive(Debug)]
pub(super) struct Held<F: Float> {
    reader: Reopened<F>,
    topology: tutti_buffers::ChannelTopology,
    announced: bool,
}

/// The taps of a [`Graph`]: copies of its audio and of one event port, read
/// through a [`TapReader`] or an [`EventTapReader`]. Made by [`Graph::taps`].
///
/// The heard tap (what the output sounds like, after the engine's declick
/// fader) is not here: it is a writer in the engine's output buffer, which
/// the graph does not own, so it is
/// [`Session::open_heard_tap`](crate::Session::open_heard_tap).
#[derive(Debug)]
pub struct Taps<'g, F: Float = f32> {
    pub(super) graph: &'g mut Graph<F>,
}

/// The level meters of a [`Graph`]: per-channel peak and energy readers on its
/// outputs or on any node's ports. Made by [`Graph::meters`].
#[derive(Debug)]
pub struct Meters<'g, F: Float = f32> {
    pub(super) graph: &'g mut Graph<F>,
}

impl<F: Float> Taps<'_, F> {
    /// Copies `sources`, one lane each, out of the graph into a ring the
    /// returned reader drains: a tap on any ports, at any width. The tap is
    /// compiled by the next commit and fed by every block that renders after
    /// it (a device, [`Session::render`](crate::Session::render) and
    /// `Session::render_frame` alike).
    /// `options` give each lane's ring capacity in frames (what the reader does
    /// not drain in time is counted, [`TapReader::take_lost`]) and the alignment.
    /// The tap rings a bell after each block it writes and when it ends, so a
    /// reader that awaits its frames ([`TapReader::notified`]) wakes on the audio
    /// thread's ring.
    ///
    /// [`TapAlign::ToOutput`] delays each lane by the compensation an output
    /// of the same source gets, so it lines up with what the outputs carry;
    /// [`TapAlign::Raw`] reads the source as it leaves its node.
    ///
    /// This is the **plan tap**: an op of the graph, so it sees the signal
    /// before the engine's declick fader and the fold to the device width, on
    /// any ports at any width. To record what the output sounds like, with the
    /// fade, open [`Session::open_heard_tap`](crate::Session::open_heard_tap)
    /// instead (the default for a recording); this is the explicit choice of
    /// the graph's own signal. A block the engine refuses (rendered silent)
    /// writes no frames here and counts none lost, since the op did not run;
    /// the heard tap writes the silence.
    ///
    /// **Because it is before the declick fader, at a transport stop it shows
    /// in a recording.** A transport-gated source (a
    /// clip, the click, a sampler voice) ends on the stop's frame, so the tap
    /// steps from the source's whole level to silence in one frame where the
    /// engine's output buffer has faded over the declick (measured: a 0.5 source gives
    /// a step of 0.5 in the tap and a largest step of about 0.001 in the
    /// output). A source that keeps running through the stop (live input, a
    /// tail) has none. A start steps alike in both: `Play` is not declicked.
    ///
    /// Every source must exist now, edits not yet committed included: a key
    /// no node has, a port past the node's width or an input past the graph's
    /// is refused with [`MissingSource`] and nothing is opened. Only the open
    /// is checked. A port that narrows or vanishes later (a node replaced by
    /// one with fewer outputs, a removed node) reads silence, the commit
    /// succeeds and the reader is not told. A latency
    /// change that resizes an aligned tap's delay shifts the stream with no
    /// marker for the reader, as it does an output. What the reader does not
    /// drain is dropped and counted, and a recording closes up over the gap.
    ///
    /// Dropping the reader leaves the tap open until
    /// [`close`](Self::close) or the session's next
    /// [`Reclaim`](crate::Phase::Reclaim) phase, which closes it.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open_ports(
        &mut self,
        sources: &[OutPort],
        options: TapOptions,
    ) -> Result<TapReader<F>, MissingSource> {
        let sources = self.graph.raw_sources(sources)?;
        let id = TapId::mint(&self.graph.ids);
        let reader = self.graph.editor.open_tap(id, sources, options)?;
        self.graph.edited = true;
        Ok(reader)
    }

    /// Opens a tap on `sources` ([`open_ports`](Self::open_ports)) and an [`Analyzer`] over
    /// it: the meter `analysis` builds, bound to the graph's rate and the tap's
    /// width, fed what the tap holds each time the app drains it
    /// ([`Analyzer::pending`]). The analysis is bound the way
    /// [`Session::analyze`](crate::Session::analyze) binds it to a wave
    /// ([`AudioContext`], [`AudioInput`]).
    ///
    /// `options` are the tap's: its capacity and its alignment. An app that
    /// awaits frames awaits [`Analyzer::notified`]. A rate change after the open
    /// is not followed.
    ///
    /// # Errors
    ///
    /// [`AnalyzerError::Measure`] when the analysis cannot be bound to the
    /// graph's rate and the tap's width, [`AnalyzerError::Bind`] when the
    /// tap's width has no view of the kind the meter takes, and
    /// [`AnalyzerError::MissingSource`] for a source that is not in the graph
    /// now. Nothing is opened on an error.
    #[cfg(feature = "analyze")]
    pub fn open_analyzer<A>(
        &mut self,
        sources: &[OutPort],
        analysis: A,
        options: TapOptions,
    ) -> Result<Analyzer<A, F>, AnalyzerError>
    where
        A: tutti_measure::Measure<F>,
        A::Context: AudioContext,
        for<'a> <A::Meter as tutti_measure::StreamMeter>::Input<'a>: AudioInput<'a, F>,
    {
        let rate = self.graph.editor.prepare().sample_rate();
        let width = u16::try_from(sources.len()).unwrap_or(u16::MAX);
        Analyzer::open(rate, width, &analysis, || self.open_ports(sources, options))
    }

    /// A tap on the graph's global outputs, aligned: lane `c` is output
    /// channel `c` as the outputs carry it, **post-sum**: a channel several
    /// sources feed is heard whole, each source delayed to the output time and
    /// added ([`TapSpec::Outputs`](tutti_graph::TapSpec::Outputs)). It follows
    /// the outputs when they are re-wired
    /// ([`set_sources`](Graph::set_sources),
    /// [`connect`](Graph::connect)); its width is the
    /// output count when it was opened, and a later
    /// [`widen_outputs`](Graph::widen_outputs) does not widen it.
    ///
    /// **A widen leaves the tap at its width, a declared main output reopens it when
    /// the widths differ.** A declaration
    /// ([`Session::declare_main_output_topology`](crate::Session::declare_main_output_topology))
    /// that leaves the main output a width other than the tap's opens the tap again at
    /// the main output's, and the session reports it as
    /// [`SessionEvent::MainOutputReaderReopened`](crate::SessionEvent::MainOutputReaderReopened);
    /// [`Session::take_reopened`](crate::Session::take_reopened) claims the new
    /// reader. A declaration that leaves the main output the tap's width leaves the tap
    /// open, however the main output got there. The new reader is fed from the first
    /// block of the plan that carries the change and never reads the old main output's
    /// channels. The old reader reads what its rings hold, none of it counted
    /// lost, and then ends ([`Pull::Ended`](tutti_buffers::Pull::Ended)), so no
    /// reader takes twelve channels for two.
    ///
    /// This is the signal the graph produces, before the engine's declick
    /// fader and the fold to the device's width; see [`open_ports`](Self::open_ports)
    /// for what that means at a transport stop. To record what is heard, with
    /// the fade, use [`Session::open_heard_tap`](crate::Session::open_heard_tap).
    ///
    /// Refuses with [`MissingSource`] when an output names a source that does
    /// not exist now (an output wired to a port the node does not have); only
    /// the open is checked, as for [`open_ports`](Self::open_ports).
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first output source that is not in the graph
    /// now.
    pub fn open_main_output(&mut self, capacity: Samples) -> Result<TapReader<F>, MissingSource> {
        let id = TapId::mint(&self.graph.ids);
        let width = self.graph.outputs();
        let reader = self.graph.editor.open_heard_tap(id, width, capacity)?;
        self.graph.output_taps.insert(id, (capacity, width));
        self.graph.edited = true;
        Ok(reader)
    }

    /// Closes a tap: its ops leave the plan at the next commit and the ring
    /// is freed on this side. Dropping the reader closes it at the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase; this closes it now, with the
    /// reader still held.
    ///
    /// # Errors
    ///
    /// [`CloseError::Tap`] when no tap `id` is open (closed already, or never
    /// opened); nothing changes.
    pub fn close(&mut self, id: TapId) -> Result<(), CloseError> {
        self.graph.editor.close_tap(id)?;
        self.graph.output_taps.remove(&id);
        self.graph.reopened.remove(&MainOutputReader::Tap(id));
        self.graph.edited = true;
        Ok(())
    }

    /// Opens a tap on one event port of a node, an output
    /// ([`Key::events_out`](crate::Key::events_out)) or an input
    /// ([`Key::events_in`](crate::Key::events_in)), and returns its reader: the
    /// MIDI events the port carries, each with the frame it was due on
    /// ([`TapEvent`](tutti_graph::TapEvent)), on the clock of the rate the
    /// graph is prepared at now ([`EventTapReader::rate`]). The tap is
    /// compiled by the next commit and fed by every block that renders after
    /// it; a copy of events, the event sibling of [`open_ports`](Self::open_ports).
    ///
    /// `options` carry the ring's capacity, a count of events
    /// ([`EventCapacity`](tutti_graph::EventCapacity), as for `Midi::tap`). A non-note-off is refused once that
    /// many are held, a note-off is kept up to
    /// [`note_off_room`](tutti_graph::note_off_room) of it, and each refusal is
    /// counted ([`EventTapReader::take_lost`]). `options`' alignment is as for a plan tap:
    /// [`TapAlign::ToOutput`] delays the port by the compensation an output of
    /// the same node gets, so the frames line up with the audio the graph
    /// outputs; [`TapAlign::Raw`] reads it as it leaves its node. Ramps,
    /// segments, harmony and resync are not copied.
    ///
    /// A tap of an input hears what the node hears there: every source of the
    /// port merged, in the order the node is handed them, after their latency
    /// delays (and, with [`TapAlign::ToOutput`], the delay the node's input
    /// time needs to reach the output time).
    ///
    /// The port must exist now, edits not yet committed included
    /// ([`MissingSource::Node`], [`MissingSource::EventPort`] or
    /// [`MissingSource::EventInPort`] otherwise, and nothing is opened). One that vanishes later reads no events, counts no
    /// loss and fails no commit. Dropping the reader leaves the tap open until
    /// [`close_event`](Self::close_event) or the session's next
    /// [`Reclaim`](crate::Phase::Reclaim) phase, which closes it.
    ///
    /// **A re-prepare at another rate moves the graph's clock to a new scale,**
    /// so a reader's frames stay on the rate it was opened at only until then;
    /// the caller that re-rates the graph reopens its taps
    /// ([`reopen_event`](Self::reopen_event)).
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for a port that is not in the graph now.
    pub fn open_event(
        &mut self,
        source: impl Into<EventTapPort>,
        options: EventTapOptions,
    ) -> Result<EventTapReader, MissingSource> {
        let id = EventTapId::mint(&self.graph.ids);
        let source = source.into();
        let node = source.node();
        let raw = match source {
            EventTapPort::Out(p) => p.raw().map(EventTapSource::Out),
            EventTapPort::In(p) => p.raw().map(EventTapSource::In),
        }
        .ok_or_else(|| {
            let shape = self.graph.shape(node);
            match source {
                EventTapPort::Out(_) => MissingSource::EventPort {
                    node,
                    port: u16::MAX,
                    width: shape.map_or(0, |s| usize::from(s.event_out)),
                },
                EventTapPort::In(_) => MissingSource::EventInPort {
                    node,
                    port: u16::MAX,
                    width: shape.map_or(0, |s| usize::from(s.event_in)),
                },
            }
        })?;
        let reader = self.graph.editor.open_event_tap(id, raw, options)?;
        self.graph.event_taps.insert(id, (raw, options));
        self.graph.edited = true;
        Ok(reader)
    }

    /// Closes an event tap: its op leaves the plan at the next commit and the
    /// ring is freed on this side. Dropping the reader closes it at the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase; this closes it now, with the
    /// reader still held.
    ///
    /// # Errors
    ///
    /// [`CloseError::EventTap`] when no event tap `id` is open; nothing
    /// changes.
    pub fn close_event(&mut self, id: EventTapId) -> Result<(), CloseError> {
        self.graph.editor.close_event_tap(id)?;
        self.graph.event_taps.remove(&id);
        self.graph.edited = true;
        Ok(())
    }

    /// Opens the event tap `id` again, at the rate the graph is prepared at
    /// now, and returns the new reader: it replaces the tap of the same
    /// [`id`](EventTapReader::id), so the reader it supersedes is fed no more.
    /// What the owner of a tap does after a re-prepare at another rate
    /// ([`set_sample_rate`](Graph::set_sample_rate); a device session's restart
    /// re-rates the graph too), so that one reader's frames are always on one
    /// scale. Each owner reopens its own tap and no other (the session's
    /// hardware MIDI out reopens its own), so another owner's reader is not
    /// superseded and left dead. `None` for an id that is not open, and for a
    /// tap whose node was removed since, which is dropped from the graph's
    /// list.
    ///
    /// The graph cannot do this by itself: a reader belongs to the thread that
    /// drains it, and the new one has to reach that thread.
    pub fn reopen_event(&mut self, id: EventTapId) -> Option<EventTapReader> {
        let &(port, options) = self.graph.event_taps.get(&id)?;
        self.graph.edited = true;
        match self.graph.editor.open_event_tap(id, port, options) {
            Ok(reader) => Some(reader),
            Err(_) => {
                self.graph.event_taps.remove(&id);
                // Open a moment ago, so there is a tap to close.
                let _ = self.graph.editor.close_event_tap(id);
                None
            }
        }
    }
}

impl<F: Float> Meters<'_, F> {
    /// Declares a level meter on `sources`, one channel each, any node
    /// ports, and returns its reader, **off** until
    /// [`enable`](MeterReader::enable). The meter is compiled by the next
    /// commit and fed by every block that renders after it (a device,
    /// [`Session::render`](crate::Session::render) and
    /// `Session::render_frame` alike).
    ///
    /// A meter reads before the engine's fold to the device and its declick.
    /// It holds the peak since the last
    /// [`read`](MeterReader::read), so a UI that polls slower than the blocks
    /// still sees a spike; ballistics are the reader's. One reader per meter:
    /// a second consumer opens a second meter.
    ///
    /// `align` is the tap's choice: [`TapAlign::Raw`] reads each source as it
    /// leaves its node (a track's level), [`TapAlign::ToOutput`] delays each
    /// lane by the compensation an output of the same source gets (a ring per
    /// lane), so it lines up with what the outputs carry.
    ///
    /// Every source must exist now, edits not yet committed included
    /// ([`MissingSource`] otherwise, and nothing is opened). Removing a node a
    /// meter reads afterwards leaves that channel on silence, and so does
    /// replacing it with a node that has fewer outputs (the lanes past the
    /// new width); the commit succeeds either way. Dropping the reader
    /// leaves the meter open until [`close`](Self::close) or the
    /// session's next [`Reclaim`](crate::Phase::Reclaim) phase, which closes
    /// it and frees its cells.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open(
        &mut self,
        sources: &[OutPort],
        align: TapAlign,
    ) -> Result<MeterReader, MissingSource> {
        let sources = self.graph.raw_sources(sources)?;
        self.declare_meter(MeterSpec::Ports { sources, align })
    }

    fn declare_meter(&mut self, spec: MeterSpec) -> Result<MeterReader, MissingSource> {
        let id = MeterId::mint(&self.graph.ids);
        let reader = self.graph.editor.open_meter(id, spec)?;
        self.graph.edited = true;
        Ok(reader)
    }

    /// Closes a meter: its op leaves the plan at the next commit and its
    /// cells are freed on this side. Dropping the reader closes it at the
    /// next [`Reclaim`](crate::Phase::Reclaim) phase; this closes it now, with
    /// the reader still held.
    ///
    /// # Errors
    ///
    /// [`CloseError::Meter`] when no meter `id` is open; nothing changes.
    pub fn close(&mut self, id: MeterId) -> Result<(), CloseError> {
        self.graph.editor.close_meter(id)?;
        self.graph.reopened.remove(&MainOutputReader::Meter(id));
        self.graph.edited = true;
        Ok(())
    }
}

impl<'g, F: Float> Meters<'g, F> {
    /// Hands the master meter's reader over
    /// ([`Session::take_main_output_meter`](crate::Session::take_main_output_meter)).
    pub(crate) fn take_main_output(&mut self) -> Option<MeterReader> {
        self.graph.main_output_meter.take()
    }
}

impl<F: Float> Graph<F> {
    /// Opens again each reader of the main output whose width is not the main output's now.
    /// A tap is reopened when its own width differs from the main output's, whatever
    /// moved the main output; the master meter follows the main output's width as it widens,
    /// so it is reopened when `resized`, the declaration having changed the
    /// main output's width. No reader keeps a width the main output no longer has.
    ///
    /// The new reader takes the old one's id, so the commit that carries the
    /// change swaps the executor's writer for the new one at that block, and
    /// the old reader ends once that commit is applied
    /// (`Editor::reopen_heard_tap`). The master meter held by the graph is
    /// replaced in place; one a host took is held under its id to be claimed.
    pub(super) fn reopen_main_output_readers(&mut self, resized: bool) {
        let width = self.outputs();
        let topology = self.main_output_in_force();
        let stale: Vec<(TapId, Samples)> = self
            .output_taps
            .iter()
            .filter(|&(_, &(_, w))| w != width)
            .map(|(&id, &(capacity, _))| (id, capacity))
            .collect();
        for (id, capacity) in stale {
            match self.editor.reopen_heard_tap(id, width, capacity) {
                Ok(reader) => {
                    self.output_taps.insert(id, (capacity, width));
                    self.hold(MainOutputReader::Tap(id), Reopened::Tap(reader), &topology);
                }
                // Closed since: no reader is left to end.
                Err(_) => {
                    self.output_taps.remove(&id);
                    self.reopened.remove(&MainOutputReader::Tap(id));
                }
            }
        }
        let id = self.main_output_meter_id;
        if resized && self.editor.spec().topology.meters.contains_key(&id) {
            // `Outputs` names no source, so the open cannot be refused.
            let reader = self
                .editor
                .open_meter(id, MeterSpec::Outputs)
                .expect("the outputs meter names no source");
            reader.enable();
            if self.main_output_meter.is_some() {
                self.main_output_meter = Some(reader);
            } else {
                self.hold(
                    MainOutputReader::Meter(id),
                    Reopened::Meter(reader),
                    &topology,
                );
            }
        }
    }

    /// Holds `reader` under `id` to be claimed, in place of one held for `id`
    /// and not claimed: that one is dropped, and the editor ended it when this
    /// one replaced it.
    fn hold(
        &mut self,
        id: MainOutputReader,
        reader: Reopened<F>,
        topology: &tutti_buffers::ChannelTopology,
    ) {
        self.reopened.insert(
            id,
            Held {
                reader,
                topology: topology.clone(),
                announced: false,
            },
        );
    }

    /// The ids reopened since the last call, each with the main output's topology it
    /// was opened for, once each until the reader under it is replaced.
    pub(crate) fn announce_reopened(
        &mut self,
    ) -> Vec<(MainOutputReader, tutti_buffers::ChannelTopology)> {
        self.reopened
            .iter_mut()
            .filter(|(_, held)| !held.announced)
            .map(|(&id, held)| {
                held.announced = true;
                (id, held.topology.clone())
            })
            .collect()
    }

    /// Claims the reader reopened under `id`: it is held until then, whether
    /// or not the event was read.
    ///
    /// # Errors
    ///
    /// [`NotReopened`] when none is held under `id`.
    pub fn take_reopened(&mut self, id: MainOutputReader) -> Result<Reopened<F>, NotReopened> {
        self.reopened
            .remove(&id)
            .map(|held| held.reader)
            .ok_or(NotReopened(id))
    }
}
