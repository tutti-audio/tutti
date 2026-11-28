//! The parts an [`InstrumentDecl`](crate::InstrumentDecl) owns beside its node,
//! which have no app key: the event queue its keys fill, wired to the node's
//! event input, and the strip after its main pair. The session keeps them by
//! the instrument's key and lends the keys once ([`Session::take_keys`]).

use thiserror::Error;
use tutti_buffers::{ChannelTopology, Float};
use tutti_graph::{HostValues, InletCounts, NodeKey, Shape};
use tutti_midi_runtime::{LIVE_INLET_CAPACITY, MidiKeys};
use tutti_nodes::{BusStripConfig, BusStripNode};
use tutti_ump::MidiGroup;
use tutti_units::Amplitude;

use super::Kept;
use super::wire::Resolved;
use crate::Graph;
use crate::patch::{Built, Fading, Place};
use crate::{AnyAssign, AppKey, Clock, EventInputPort, Key, OutputPort, ReplaceRefused, Session};

/// Who plays an instrument: its keys wait in the session until taken, and a
/// writer that was dropped is replaced by a new queue on the next take.
enum Lease {
    Held(MidiKeys),
    Taken,
}

/// What the last patch made beside the node of an instrument key.
pub(super) struct Parts {
    queue: NodeKey,
    strip: NodeKey,
    /// The pair of the node's outputs the strip reads.
    main: [u16; 2],
    keys: Lease,
    /// The queue's writer, as the session watches it from here.
    writer: InletCounts,
}

impl Parts {
    pub(super) fn strip(&self) -> NodeKey {
        self.strip
    }

    /// The queue and the strip, the nodes a removal takes out.
    pub(super) fn nodes(&self) -> [NodeKey; 2] {
        [self.queue, self.strip]
    }

    /// The write that sets the strip's volume to `gain`.
    pub(super) fn set_gain(&self, gain: Amplitude) -> AnyAssign {
        Key::<BusStripNode>::new(self.strip)
            .volume()
            .to(gain)
            .erase()
    }
}

/// The parts a declaration makes new: the strip, built once in the plan. The
/// queue is opened where the node is placed.
pub(super) struct NewParts {
    strip: NodeKey,
    built: Built,
}

impl NewParts {
    /// A stereo strip at `gain`, under a key no node holds.
    pub(super) fn new<F: Float>(gain: Amplitude) -> Self {
        let config = BusStripConfig::new(ChannelTopology::STEREO).volume(gain);
        Self {
            strip: NodeKey::fresh(),
            built: <Graph<F> as Place<BusStripConfig>>::build(&config, &HostValues::new()),
        }
    }

    pub(super) fn strip(&self) -> NodeKey {
        self.strip
    }

    /// The strip's shape, which the wires named after the instrument are
    /// checked against.
    pub(super) fn shape(&self) -> Shape {
        self.built.shape
    }

    /// The wires from `node`'s main pair into the strip.
    pub(super) fn audio(&self, node: NodeKey, main: [u16; 2]) -> [Resolved; 2] {
        audio(node, self.strip, main)
    }
}

/// The wires from `node`'s main pair into `strip`, left and right.
fn audio(node: NodeKey, strip: NodeKey, main: [u16; 2]) -> [Resolved; 2] {
    let strip = Key::<BusStripNode>::new(strip);
    [0, 1].map(|side| {
        Resolved::Channel(
            OutputPort::new(node, usize::from(main[side])),
            strip.input(side),
            None,
        )
    })
}

/// Why [`Session::take_keys`] lent no keys.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LeaseError<K> {
    /// No patch declared the key.
    #[error("{0:?} is not declared")]
    Undeclared(K),
    /// The key is declared as a node, not as an instrument.
    #[error("{0:?} is declared as a node, not an instrument")]
    NotInstrument(K),
    /// The keys were taken and the app still holds them.
    #[error("the keys of {0:?} are taken and not dropped")]
    Taken(K),
    /// The queue of dropped keys could not be replaced.
    #[error("the queue of {0:?} could not be replaced: {1}")]
    Replace(K, ReplaceRefused),
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Lends the keys of the instrument declared under `key`: the producer of
    /// its event queue, which plays it on the Group the instrument listens to.
    /// The session holds them from the patch that declares the instrument
    /// until this call, and the app holds them until it drops them; then a
    /// call lends a new pair, and the old queue's pushes report
    /// [`PushError::Disconnected`](tutti_rt::PushError::Disconnected).
    ///
    /// The keys' life is the instrument's: once a patch no longer declares the
    /// key, or declares it as another kind of node, a push through them reports
    /// the same. A swap of the instrument's own node (another structure) keeps
    /// them, and they play the new node.
    ///
    /// Live keys are a performance and are not part of a bounce (see
    /// [`MidiKeys`]).
    ///
    /// # Errors
    ///
    /// [`LeaseError::Undeclared`] and [`LeaseError::NotInstrument`] for a key
    /// that no patch declares as an instrument, [`LeaseError::Taken`] while the
    /// app holds the keys it was lent, and [`LeaseError::Replace`] when the
    /// graph would not take the new queue (it is poisoned).
    pub fn take_keys(&mut self, key: &K) -> Result<MidiKeys, LeaseError<K>> {
        let Some(parts) = self.declared.instruments.get_mut(key) else {
            return Err(if self.declared.nodes.contains_key(key) {
                LeaseError::NotInstrument(key.clone())
            } else {
                LeaseError::Undeclared(key.clone())
            });
        };
        if let Lease::Held(keys) = std::mem::replace(&mut parts.keys, Lease::Taken) {
            return Ok(keys);
        }
        if !parts.writer.writer_gone() {
            return Err(LeaseError::Taken(key.clone()));
        }
        // The keys were dropped: a new queue takes the old one's place, wired
        // as it was.
        let writer = self
            .graph
            .reopen_event_inlet(Key::new(parts.queue), LIVE_INLET_CAPACITY)
            .map_err(|refused| LeaseError::Replace(key.clone(), refused))?;
        parts.writer = writer.counts();
        Ok(MidiKeys::new(writer, MidiGroup::FIRST))
    }

    /// Places `new` beside `node`, the instrument declared under `key`, and
    /// wires the queue into its event input and its main pair into the strip.
    /// The wires were checked against the node's shape.
    pub(super) fn add_parts(
        &mut self,
        kept: &mut Kept<K, C::Float>,
        key: K,
        node: NodeKey,
        main: [u16; 2],
        new: NewParts,
    ) {
        let NewParts { strip, built } = new;
        let (queue, writer) = self.graph.open_event_inlet(LIVE_INLET_CAPACITY);
        let writer_view = writer.counts();
        <Graph<C::Float> as Place<BusStripConfig>>::add(&mut self.graph, strip, built);
        let queue_out = queue.event_output(0);
        self.graph
            .add_event_edge(queue_out, EventInputPort::new(node, 0), None)
            .expect("an instrument's event input was checked in the plan");
        let parts = Parts {
            queue: queue.erase(),
            strip,
            main,
            keys: Lease::Held(MidiKeys::new(writer, MidiGroup::FIRST)),
            writer: writer_view,
        };
        self.join_audio(&parts, node);
        kept.instruments.insert(key, parts);
    }

    /// Connects the strip to the node's main pair, with no fade: the strip
    /// is not heard before it is.
    fn join_audio(&mut self, parts: &Parts, node: NodeKey) {
        for wire in audio(node, parts.strip, parts.main) {
            wire.connect(&mut self.graph, true, None)
                .expect("an instrument's main pair was checked in the plan");
        }
    }

    /// Wires the strip to the main pair of `node`, now another structure, when
    /// the pair changed.
    pub(super) fn rejoin(&mut self, parts: &mut Parts, node: NodeKey, main: [u16; 2]) {
        if parts.main == main {
            return;
        }
        for wire in audio(node, parts.strip, parts.main) {
            let _ = wire.disconnect(&mut self.graph, true);
        }
        parts.main = main;
        self.join_audio(parts, node);
    }

    /// Takes the queue and the strip out of the session over the fade the
    /// instrument's declaration chose; the lease drops with them.
    pub(super) fn take_out_parts(&mut self, parts: &Parts, fading: Fading) {
        for node in parts.nodes() {
            if let Ok(removal) = self.remove(node)
                && fading == Fading::Cut
            {
                let _ = removal.unfaded();
            }
        }
    }
}
