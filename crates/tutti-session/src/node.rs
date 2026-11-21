//! [`SessionNode`], the trait [`Graph::add`](crate::Graph::add), the graph's one
//! way in, takes, and the engine's own nodes as ones.

use tutti_buffers::Float;
use tutti_graph::{ForkByClone, IntoNode, Node, NodeKey, ParamNode, ParamSet, Params, Unforkable};

use crate::NodeSlots;

/// A node that may enter a session's graph: it declares its own ports (event
/// ports included), its controls and its fork ([`IntoNode`]), and says which
/// of its controls is a [`ParamSet`] ([`params`](Self::params)).
///
/// Every node the engine ships is one. A host's own node is one of:
///
/// - wrapped: [`ForkByClone`]`(node)` (forkable by a clone taken at insert,
///   for a node whose `Clone` shares nothing) or [`Unforkable`]`(node)`
///   (refuses a fork that needs it): no controls and no params;
/// - a `tutti_graph::ParamNode` (its params a `ParamSet`, forked by
///   `tutti_graph::param_parts`), whose impl returns its controls from
///   `params`: its params reached by address
///   ([`Session::set`](crate::Session::set) on a [`ParamTarget`](crate::ParamTarget), control-rate
///   modulation) and forked from what was set;
/// - its own impl with the default `params` (none), for a node with
///   controls of another shape.
///
/// The controls are `Send + 'static`: the graph owns them for the node's life
/// ([`Graph::add`](crate::Graph::add)) and drops them once the node has left
/// the render.
///
/// Every impl states what frees what its controls publish
/// ([`reclaimer`](Self::reclaimer)): [`Reclaims::Cells`] for a node whose
/// controls publish to it through a `Publisher` cell (or hold another
/// control-side free), [`Reclaims::Nothing`] for one that publishes nothing.
/// There is no default, so an impl that forgets the sweep does not compile and
/// the session's reclaim phase reaches every node whichever host inserted it.
///
/// A node that says nothing about its sweep is refused (E0046):
///
/// ```compile_fail,E0046
/// use tutti_graph::{IntoNode, NodeParts};
/// use tutti_session::SessionNode;
///
/// struct Mine;
/// impl IntoNode for Mine {
///     type Controls = ();
///     fn into_parts(self) -> NodeParts<()> {
///         unimplemented!()
///     }
/// }
/// impl SessionNode for Mine {}
/// ```
///
/// One that publishes nothing says so by name:
///
/// ```
/// use tutti_graph::{IntoNode, NodeParts};
/// use tutti_session::{Reclaims, SessionNode};
///
/// struct Mine;
/// impl IntoNode for Mine {
///     type Controls = ();
///     fn into_parts(self) -> NodeParts<()> {
///         unimplemented!()
///     }
/// }
/// impl SessionNode for Mine {
///     fn reclaimer(_: &()) -> Reclaims {
///         Reclaims::Nothing
///     }
/// }
/// ```
pub trait SessionNode<F: Float = f32>:
    IntoNode<F, Controls: Send + 'static> + Send + 'static
{
    /// The node's params by address, read off its controls once it is in:
    /// what a [`ParamTarget`](crate::ParamTarget) write goes through, and
    /// what a fork of the node starts from. `None`, the default, for a node
    /// with none.
    fn params(controls: &Self::Controls) -> Option<ParamSet> {
        let _ = controls;
        None
    }

    /// The node's other values by what they are, read off its controls once
    /// it is in: the cells a slot target (`clip.events()`) writes through.
    /// They are kept under `node`, and forgotten when the node is removed or
    /// replaced. Empty, the default, for a node with none.
    fn slots(node: NodeKey, controls: &Self::Controls) -> NodeSlots {
        let _ = (node, controls);
        NodeSlots::default()
    }

    /// What frees the values the node's published cells retired, read off
    /// its controls once it is in. The graph keeps it by node until the node
    /// is removed or replaced, and runs it on every update
    /// ([`Phase::Reclaim`](crate::Phase::Reclaim)). Required:
    /// [`Reclaims::Nothing`] says the node publishes nothing.
    fn reclaimer(controls: &Self::Controls) -> Reclaims;

    /// The node reading the session's tempo map through `tempo` (a reader
    /// the graph hands each node it takes in [`Graph::add`](crate::Graph::add)
    /// and [`Graph::crossfade`](crate::Graph::crossfade); the node clones it).
    /// The identity, the default, for a node that reads no map: a sampler
    /// node overrides it, so a placed voice stays where the map puts it
    /// through a step or a ramp (doc 020, stage 5).
    #[must_use]
    fn bind_tempo(self, tempo: &tutti_timeline::TempoReader) -> Self
    where
        Self: Sized,
    {
        let _ = tempo;
        self
    }
}

/// What frees the values a node's published cells retired: the answer
/// [`SessionNode::reclaimer`] must give.
#[derive(Debug)]
pub enum Reclaims {
    /// The node's controls publish; this sweeps them.
    Cells(Reclaimer),
    /// The node's controls publish nothing that needs a sweep.
    Nothing,
}

impl Reclaims {
    /// The sweep over a clone of `controls` ([`Reclaimer::of`]).
    pub fn of<R: tutti_rt::Reclaim + Clone + Send + 'static>(controls: &R) -> Self {
        Self::Cells(Reclaimer::of(controls))
    }

    /// The sweep, if there is one.
    pub(crate) fn into_reclaimer(self) -> Option<Reclaimer> {
        match self {
            Self::Cells(reclaimer) => Some(reclaimer),
            Self::Nothing => None,
        }
    }
}

/// A node's control-side reclaim, kept by the graph
/// ([`SessionNode::reclaimer`]): a clone of the node's controls, which
/// implement [`Reclaim`](tutti_rt::Reclaim) (each `Publisher::reclaim`), and how
/// many retired values are still pinned after it runs. Control thread only,
/// like the `reclaim` it calls.
pub struct Reclaimer(Box<dyn tutti_rt::Reclaim + Send>);

impl Reclaimer {
    /// A reclaimer that sweeps a clone of `controls`. The clone is the whole
    /// controls, so what they keep alive (a token, a second cell) lives as long
    /// as the reclaimer does.
    pub fn of<R: tutti_rt::Reclaim + Clone + Send + 'static>(controls: &R) -> Self {
        Self(Box::new(controls.clone()))
    }

    /// Runs it: how many retired values are still pinned.
    pub fn reclaim(&self) -> usize {
        self.0.reclaim()
    }
}

impl std::fmt::Debug for Reclaimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reclaimer(..)")
    }
}

/// Each listed type is a [`SessionNode`] whose controls are its
/// [`ParamSet`]. The params it has, and the unit of each, are the node's own
/// declaration (`tutti_graph::declare_params!`, or its control set's), from
/// which its [`Key`](crate::Key)'s param methods come.
macro_rules! param_session_node {
    // A node of a float, one per width a graph has.
    (floats: $($ty:ident),* $(,)?) => {$(
        param_session_node!(@impl f32, tutti_nodes::$ty<f32>);
        param_session_node!(@impl f64, tutti_nodes::$ty<f64>);
    )*};
    // A node of no float of its own, in a graph of any.
    (any: $($ty:ty),* $(,)?) => {$(
        impl<F: Float> SessionNode<F> for $ty {
            fn params(controls: &ParamSet) -> Option<ParamSet> {
                Some(controls.clone())
            }

            // A `ParamSet` publishes through `Param` atomics, no cell.
            fn reclaimer(_: &ParamSet) -> Reclaims {
                Reclaims::Nothing
            }
        }
    )*};
    (@impl $float:ident, $ty:ty) => {
        impl SessionNode<$float> for $ty {
            fn params(controls: &ParamSet) -> Option<ParamSet> {
                Some(controls.clone())
            }

            // A `ParamSet` publishes through `Param` atomics, no cell.
            fn reclaimer(_: &ParamSet) -> Reclaims {
                Reclaims::Nothing
            }
        }
    };
}

/// A node forkable by a clone taken at insert: no controls, no params.
impl<F: Float, N: Node<F> + Clone + Send + 'static> SessionNode<F> for ForkByClone<N> {
    fn reclaimer(_: &()) -> Reclaims {
        Reclaims::Nothing
    }
}

/// A node no fork may take: no controls, no params.
impl<F: Float, N: Node<F> + Send + 'static> SessionNode<F> for Unforkable<N> {
    fn reclaimer(_: &()) -> Reclaims {
        Reclaims::Nothing
    }
}

/// A host's [`ParamNode`], inserted as one: its [`ParamSet`] as its controls
/// and its params reached by address, forked from the values set. Its typed
/// param methods come from its own `tutti_graph::declare_params!`.
impl<F: Float, N: ParamNode<F> + Clone + Send + 'static> SessionNode<F> for Params<N> {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }

    fn reclaimer(_: &ParamSet) -> Reclaims {
        Reclaims::Nothing
    }
}

param_session_node!(
    floats: SvfFilterNode,
    EqBandNode,
    LadderFilterNode,
    CompressorNode,
    GateNode,
    LimiterNode,
    BrickwallLimiterNode,
    DistortionNode,
    CrossoverNode,
);

/// The bus strip: its params by address, in a graph of any float.
impl<F: Float> SessionNode<F> for tutti_nodes::BusStripNode {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }

    fn reclaimer(_: &ParamSet) -> Reclaims {
        Reclaims::Nothing
    }
}

/// The multiband compressor: its params by address, its band meters beside
/// them in its controls.
macro_rules! multiband_session_node {
    ($($float:ty),*) => {$(
        impl SessionNode<$float> for tutti_nodes::MultibandCompressorNode<$float> {
            fn params(controls: &tutti_nodes::MultibandControls) -> Option<ParamSet> {
                Some(controls.params().clone())
            }

            fn reclaimer(_: &tutti_nodes::MultibandControls) -> Reclaims {
                Reclaims::Nothing
            }
        }
    )*};
}

multiband_session_node!(f32, f64);

// The delay, modulation, LFO and oscillator nodes, and the three reverbs (wet only: their
// blend is the graph's mix), and the convolution reverb (its wet gain, `Volume`).
param_session_node!(
    floats: DelayNode,
    ModDelayNode,
    PhaserNode,
    FreeverbNode,
    FdnNode,
    ShimmerNode,
    PlateReverbNode,
    SpringReverbNode,
    ConvolverNode,
    NoiseNode,
);
param_session_node!(any: tutti_nodes::LfoNode, tutti_nodes::OscillatorNode);

/// An audio inlet as a graph node: no controls (the writer is the producer's),
/// no params. Its fork is refused ([`Graph::open_inlet`](crate::Graph::open_inlet)).
impl<F: Float> SessionNode<F> for tutti_graph::InletNode<F> {
    fn reclaimer(_: &()) -> Reclaims {
        Reclaims::Nothing
    }
}

/// An event inlet as a graph node: no controls (the writer is the producer's),
/// no params; its fork is silent
/// ([`Graph::open_event_inlet`](crate::Graph::open_event_inlet)).
impl<F: Float> SessionNode<F> for tutti_graph::EventInletNode {
    fn reclaimer(_: &()) -> Reclaims {
        Reclaims::Nothing
    }
}

/// The metronome as a graph node: its settings are shared through the
/// transport, no controls and no params.
impl<F: Float> SessionNode<F> for tutti_nodes::ClickNode<F> {
    fn reclaimer(_: &<Self as IntoNode<F>>::Controls) -> Reclaims {
        Reclaims::Nothing
    }
}

/// The transport sent as MIDI, as a graph node: its controls are its
/// settings and losses, no params.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::SyncOut {
    fn reclaimer(_: &<Self as IntoNode<F>>::Controls) -> Reclaims {
        Reclaims::Nothing
    }
}

/// An automation lane as a graph node: no controls, the beat read from its
/// block's `Env`.
impl<F: Float> SessionNode<F> for tutti_nodes::automation::AutomationLaneNode {
    fn reclaimer(_: &()) -> Reclaims {
        Reclaims::Nothing
    }
}

/// A MIDI clip as a graph node: its `MidiClipControls`, no params; its
/// events are a slot (`clip.events()`), and the clips it replaced are freed
/// by its reclaimer.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::MidiClipNode {
    fn slots(node: NodeKey, controls: &tutti_midi_runtime::MidiClipControls) -> NodeSlots {
        NodeSlots::of(
            crate::value::SlotId::ClipEvents(node),
            crate::value::PublishedSlot::new(controls.clone()),
        )
    }

    fn reclaimer(controls: &tutti_midi_runtime::MidiClipControls) -> Reclaims {
        Reclaims::of(controls)
    }
}

/// The MPE transform: its `MpeTransformControls` read its losses, no params;
/// its mode is the session's cell, which the session sweeps.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::MpeTransformNode {
    fn reclaimer(_: &tutti_midi_runtime::MpeTransformControls) -> Reclaims {
        Reclaims::Nothing
    }
}

/// The harmony node: its `HarmonyControls`, no params; its changes are a slot
/// (`harmony.changes()`), and the lanes it replaced are freed by its
/// reclaimer.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::HarmonyNode {
    fn slots(node: NodeKey, controls: &tutti_midi_runtime::HarmonyControls) -> NodeSlots {
        NodeSlots::of(
            crate::value::SlotId::Harmony(node),
            crate::value::PublishedSlot::new(controls.clone()),
        )
    }

    fn reclaimer(controls: &tutti_midi_runtime::HarmonyControls) -> Reclaims {
        Reclaims::of(controls)
    }
}

/// The MIDI input node: its `MidiInputControls`, no params; the Profile zones
/// it replaced freed by its reclaimer.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::MidiInputNode {
    fn reclaimer(controls: &tutti_midi_runtime::MidiInputControls) -> Reclaims {
        Reclaims::of(controls)
    }
}

/// The SoundFont player as a graph node.
#[cfg(feature = "soundfont")]
impl<F: Float + tutti_dsp::SimdLanes> SessionNode<F> for tutti_soundfont::SoundFontUnit<F> {
    fn reclaimer(_: &()) -> Reclaims {
        Reclaims::Nothing
    }
}

// The two panners: bearing and height, and the VBAP spread and width or the
// HRTF blend, as a `ParamSet`.
param_session_node!(floats: VbapPannerNode, HrtfBinauralNode);

// The synth as a graph node: one MIDI event input, and its live params
// (master volume, unison detune and spread) as a `ParamSet`, so a param
// write reaches it and a fork of it starts from what was set.
#[cfg(feature = "synth")]
impl<F: tutti_polysynth::SynthFloat> SessionNode<F> for tutti_polysynth::PolySynth<F> {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }

    // A `ParamSet` publishes through `Param` atomics, no cell.
    fn reclaimer(_: &ParamSet) -> Reclaims {
        Reclaims::Nothing
    }
}

/// The values of a voice node that the session's verbs reach: its window,
/// loop, direction and speed, each a published cell of its handle.
#[cfg(feature = "sampler")]
fn voice_slots<F: tutti_sampler::SamplerFloat>(
    node: NodeKey,
    controls: &tutti_sampler::VoiceNodeHandle<F>,
) -> NodeSlots {
    use crate::value::{PublishedSlot, SlotId, SpeedCell};
    NodeSlots::of(
        SlotId::Window(node),
        PublishedSlot::new(controls.window_cell().clone()),
    )
    .and(
        SlotId::Looping(node),
        PublishedSlot::new(controls.looping_cell().clone()),
    )
    .and(
        SlotId::Direction(node),
        PublishedSlot::new(controls.direction_cell().clone()),
    )
    .and(
        SlotId::Speed(node),
        PublishedSlot::new(SpeedCell(controls.speed_cell().clone())),
    )
}

/// A voice as a graph node: its params (the gain) addressed by its set, so a
/// param write reaches the cell the node reads and a fork starts from the
/// gain as set; the placements it replaced freed by its reclaimer.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::VoiceNode<F> {
    fn bind_tempo(self, tempo: &tutti_timeline::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn params(controls: &tutti_sampler::VoiceNodeHandle<F>) -> Option<ParamSet> {
        Some(controls.params().clone())
    }

    fn slots(node: NodeKey, controls: &tutti_sampler::VoiceNodeHandle<F>) -> NodeSlots {
        voice_slots(node, controls)
    }

    fn reclaimer(controls: &tutti_sampler::VoiceNodeHandle<F>) -> Reclaims {
        Reclaims::of(controls)
    }
}

/// A pool as a graph node; its controls are its `VoicePoolHandle`, whose
/// retired voices its reclaimer frees.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::VoicePool<F> {
    fn bind_tempo(self, tempo: &tutti_timeline::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn reclaimer(controls: &tutti_sampler::VoicePoolHandle<F>) -> Reclaims {
        Reclaims::of(controls)
    }
}

/// A bare clip reader as a graph node, its gain addressed by its set.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::MemorySource<F> {
    fn bind_tempo(self, tempo: &tutti_timeline::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn reclaimer(_: &ParamSet) -> Reclaims {
        Reclaims::Nothing
    }

    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

/// A [`Clip`](tutti_sampler::Clip) as a graph node: the voice node it
/// builds, its gain addressed by its set, and the placements it replaced (and
/// a streamed clip's stream) kept by its reclaimer for as long as the node is
/// in the graph.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::Clip<F> {
    fn bind_tempo(self, tempo: &tutti_timeline::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn params(controls: &tutti_sampler::VoiceNodeHandle<F>) -> Option<ParamSet> {
        Some(controls.params().clone())
    }

    fn slots(node: NodeKey, controls: &tutti_sampler::VoiceNodeHandle<F>) -> NodeSlots {
        voice_slots(node, controls)
    }

    fn reclaimer(controls: &tutti_sampler::VoiceNodeHandle<F>) -> Reclaims {
        Reclaims::of(controls)
    }
}

/// A streamed voice as a graph node; its controls are its
/// `DiskVoiceControls`.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::DiskVoice<F> {
    fn bind_tempo(self, tempo: &tutti_timeline::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn reclaimer(_: &tutti_sampler::DiskVoiceControls) -> Reclaims {
        Reclaims::Nothing
    }
}

/// The sampler nodes whose controls are a handle with commands:
/// [`Graph::handle`](crate::Graph::handle) reaches it by key. Sealed to the
/// four nodes whose commands are not yet targets.
#[cfg(feature = "sampler")]
pub trait Handled<F: Float = f32>: SessionNode<F> + sealed::Sealed {}

#[cfg(feature = "sampler")]
mod sealed {
    use tutti_sampler::{Clip, DiskVoice, SamplerFloat, VoiceNode, VoicePool};

    pub trait Sealed {}
    impl<F: SamplerFloat> Sealed for VoiceNode<F> {}
    impl<F: SamplerFloat> Sealed for Clip<F> {}
    impl<F: SamplerFloat> Sealed for VoicePool<F> {}
    impl<F: SamplerFloat> Sealed for DiskVoice<F> {}
}

#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> Handled<F> for tutti_sampler::VoiceNode<F> {}
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> Handled<F> for tutti_sampler::Clip<F> {}
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> Handled<F> for tutti_sampler::VoicePool<F> {}
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> Handled<F> for tutti_sampler::DiskVoice<F> {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `N`'s impl hands the graph a reclaimer once `node` is split.
    fn keeps_one<N: SessionNode>(node: N) -> bool {
        let (_, controls) = node.into_node();
        N::reclaimer(&controls).into_reclaimer().is_some()
    }

    /// **Every engine node whose controls publish to it keeps a
    /// reclaimer**: the clip, the harmony track, the live input, the pool and
    /// the voice (a `Clip`'s impl returns the same
    /// `VoiceNodeHandle` sweep as the voice's). What each one frees is pinned
    /// with a held loan in its own crate; the graph's sweep of them in
    /// `tests/wiring/graph.rs`.
    #[test]
    fn every_publishing_node_keeps_a_reclaimer() {
        #[cfg(feature = "midi")]
        {
            assert!(keeps_one(tutti_midi_runtime::MidiClipNode::new([])));
            assert!(keeps_one(tutti_midi_runtime::HarmonyNode::new([])));
            assert!(keeps_one(tutti_midi_runtime::MidiInputNode::new()));
        }
        #[cfg(feature = "sampler")]
        {
            use std::sync::Arc;
            let source = tutti_sampler::MemorySource::placed(
                Arc::new(tutti_buffers::Wave::new(1, tutti_units::SampleRate::SR_48K)),
                tutti_sampler::VoiceWindow::from(tutti_units::Beat::new(0.0)),
            )
            .expect("a placeable wave");
            assert!(keeps_one(tutti_sampler::VoicePool::new()));
            assert!(keeps_one(tutti_sampler::VoiceNode::new(
                tutti_sampler::Voice {
                    source: tutti_sampler::VoiceSource::Memory(source),
                    play: tutti_sampler::Playback::default(),
                }
            )));
        }
        // A node that publishes nothing keeps none.
        assert!(!keeps_one(
            tutti_nodes::automation::AutomationLaneNode::from(
                tutti_nodes::automation::AutomationLaneConfig::new(
                    tutti_nodes::automation::Trajectory::<tutti_units::Hz>::new()
                )
            )
        ));
    }
}
