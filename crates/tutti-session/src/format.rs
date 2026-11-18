//! [`Sealed`]: the steps of a session that only the `f32` session has.
//!
//! A private module with a public trait, so [`SessionFloat`](crate::SessionFloat)
//! names it and no other crate can implement it.

use std::sync::Arc;

use tutti_buffers::Float;
use tutti_graph::NodeKey;
use tutti_nodes::{ClickNode, ClickSettings};
use tutti_transport::Transport;

use crate::graph::Graph;
use crate::{AppKey, Clock, PhaseCx, Session, SessionEvent};

/// What the session builder inserts into a new graph beyond the host's own
/// nodes.
#[derive(Debug)]
pub struct EngineNodes {
    /// The metronome's node.
    pub(crate) click: Option<NodeKey>,
    /// The engine's MIDI nodes.
    #[cfg(feature = "midi")]
    pub(crate) midi: Option<crate::MidiNodes>,
}

/// A point in a phase where an `f32` session has work an `f64` one does not.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// What the session's jobs and the pool landed, and the removals' leftovers.
    Landings,
    /// What an export left behind.
    Exports,
    /// Modulation applied.
    Modulation,
    /// The declarations re-applied after the commit.
    Reapply,
    /// MIDI out.
    MidiOut,
    /// The plugins served.
    Plugins,
    /// The render mode announced.
    RenderMode,
    /// The MIDI devices and the tempo the session follows.
    Devices,
    /// The output devices' listing.
    Listing,
}

/// The part of a session that depends on its float format being `f32`: the
/// nodes the engine inserts and the phases that serve them.
pub trait Sealed: Float {
    /// The engine's own nodes in a new `graph`: the metronome, and (`midi`)
    /// the sync node that feeds the outputs.
    #[doc(hidden)]
    fn engine_nodes(
        graph: &mut Graph<Self>,
        transport: &Transport,
        click_settings: &Arc<ClickSettings>,
        #[cfg(feature = "midi")] input_ports: tutti_midi_runtime::MidiInputPorts,
    ) -> EngineNodes;

    /// The `f32`-only part of `step` of a phase: all of it on an `f32` session,
    /// none on an `f64` one.
    #[doc(hidden)]
    fn host<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        step: Step,
        cx: PhaseCx<'_>,
        out: &mut Vec<SessionEvent>,
    );
}

impl Sealed for f32 {
    fn engine_nodes(
        graph: &mut Graph<f32>,
        transport: &Transport,
        click_settings: &Arc<ClickSettings>,
        #[cfg(feature = "midi")] input_ports: tutti_midi_runtime::MidiInputPorts,
    ) -> EngineNodes {
        // The metronome. It reads the beat, play and record state from its
        // block's `Env`, and the count-in flag off the transport (read only).
        // It is not wired to the output: what it feeds is the host's
        // declaration, like every other node's.
        let (click, _settings) = graph.add(ClickNode::new(transport, Arc::clone(click_settings)));

        // The engine's MIDI as graph nodes: the sync node (sends nothing until a
        // host sets its messages). An input port's ingest node and an output's
        // node are made when their pool entries load, and the sync node feeds
        // each output.
        #[cfg(feature = "midi")]
        let midi = {
            let meter = click_settings.project_meter();
            let (sync, sync_controls) = graph.add(
                tutti_midi_runtime::SyncOut::new(tutti_ump::MidiGroup::FIRST)
                    .meter(move || meter.reader()),
            );
            Some(crate::MidiNodes {
                input_ports,
                sync: sync.erase(),
                sync_controls,
            })
        };
        EngineNodes {
            click: Some(click.erase()),
            #[cfg(feature = "midi")]
            midi,
        }
    }

    fn host<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        step: Step,
        cx: PhaseCx<'_>,
        out: &mut Vec<SessionEvent>,
    ) {
        session.host_step(step, cx, out);
    }
}

impl Sealed for f64 {
    fn engine_nodes(
        _: &mut Graph<f64>,
        _: &Transport,
        _: &Arc<ClickSettings>,
        #[cfg(feature = "midi")] _: tutti_midi_runtime::MidiInputPorts,
    ) -> EngineNodes {
        EngineNodes {
            click: None,
            #[cfg(feature = "midi")]
            midi: None,
        }
    }

    fn host<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        _: Step,
        _: PhaseCx<'_>,
        _: &mut Vec<SessionEvent>,
    ) {
    }
}
