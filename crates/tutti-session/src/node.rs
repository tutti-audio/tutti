//! [`SessionNode`], the trait [`Graph::add`](crate::Graph::add), the graph's one
//! way in, takes, and the engine's own nodes as ones.

use tutti_buffers::Float;
use tutti_graph::{ForkByClone, IntoNode, Node, ParamNode, Params, Unforkable};

use crate::TimelineView;

/// A node that may enter a session's graph: it declares its own ports (event
/// ports included), its controls and its fork ([`IntoNode`]), and, in its
/// [`NodeParts`](tutti_graph::NodeParts), its params by address
/// ([`NodeParts::params`](tutti_graph::NodeParts::params)), its published values
/// ([`NodeParts::published`](tutti_graph::NodeParts::published)) and what frees
/// what its cells retired
/// ([`NodeParts::reclaimer`](tutti_graph::NodeParts::reclaimer)).
/// What the trait adds is what only the session can say of a node's type: the
/// timeline it follows ([`bind_timeline`](Self::bind_timeline)), which
/// defaults to none.
///
/// Every node the engine ships is one. A host's own node is one of:
///
/// - wrapped: [`ForkByClone`]`(node)` (forkable by a clone taken at insert,
///   for a node whose `Clone` shares nothing) or [`Unforkable`]`(node)`
///   (refuses a fork that needs it): no controls and no params;
/// - a `tutti_graph::ParamNode` (its params a `ParamSet`, forked by
///   `tutti_graph::param_parts`): its params reached by address
///   ([`Session::set`](crate::Session::set) on a [`ParamTarget`](crate::ParamTarget), control-rate
///   modulation) and forked from what was set;
/// - its own `IntoNode`, for a node with controls of another shape.
///
/// The controls are `Send + 'static`: the graph owns them for the node's life
/// ([`Graph::add`](crate::Graph::add)) and drops them once the node has left
/// the render.
///
/// ```
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
pub trait SessionNode<F: Float = f32>:
    IntoNode<F, Controls: Send + 'static> + Send + 'static
{
    /// The node reading the session's timeline through `timeline` (a read-only view the
    /// graph hands each node it takes in [`Graph::add`](crate::Graph::add) and
    /// [`Graph::replace`](crate::Graph::replace); the node clones the readers it
    /// follows). The identity, the default, for a node that reads none: a sampler pool
    /// takes the loop, so the stretched voices under it carry the spare vocoder they
    /// prime a wrap with. The tempo map reaches a node through `Cx::tempo`, lent with
    /// each block.
    #[must_use]
    fn bind_timeline(self, timeline: &TimelineView) -> Self
    where
        Self: Sized,
    {
        let _ = timeline;
        self
    }
}

/// Each listed type is a [`SessionNode`] whose controls are its
/// `ParamSet`. The params it has, and the unit of each, are the node's own
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

        }
    )*};
    (@impl $float:ident, $ty:ty) => {
        impl SessionNode<$float> for $ty {

        }
    };
}

/// A node forkable by a clone taken at insert: no controls, no params.
impl<F: Float, N: Node<F> + Clone + Send + 'static> SessionNode<F> for ForkByClone<N> {}

/// A node no fork may take: no controls, no params.
impl<F: Float, N: Node<F> + Send + 'static> SessionNode<F> for Unforkable<N> {}

/// A host's [`ParamNode`], inserted as one: its `ParamSet` as its controls
/// and its params reached by address, forked from the values set. Its typed
/// param methods come from its own `tutti_graph::declare_params!`.
impl<F: Float, N: ParamNode<F> + Clone + Send + 'static> SessionNode<F> for Params<N> {}

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
impl<F: Float> SessionNode<F> for tutti_nodes::BusStripNode {}

/// The multiband compressor: its params by address, its band meters beside
/// them in its controls.
macro_rules! multiband_session_node {
    ($($float:ty),*) => {$(
        impl SessionNode<$float> for tutti_nodes::MultibandCompressorNode<$float> {

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
    WavetableNode,
);
param_session_node!(any: tutti_nodes::LfoNode, tutti_nodes::OscillatorNode);

/// An audio inlet as a graph node: no controls (the writer is the producer's),
/// no params. Its fork is refused ([`Graph::add_inlet`](crate::Graph::add_inlet)).
impl<F: Float> SessionNode<F> for tutti_graph::InletNode<F> {}

/// An event inlet as a graph node: no controls (the writer is the producer's),
/// no params; its fork is silent
/// ([`Graph::open_event_inlet`](crate::Graph::open_event_inlet)).
impl<F: Float> SessionNode<F> for tutti_graph::EventInletNode {}

/// The metronome as a graph node: its settings are shared through the
/// transport, no controls and no params.
impl<F: Float> SessionNode<F> for tutti_nodes::ClickNode<F> {}

/// The transport sent as MIDI, as a graph node: its controls are its
/// settings and losses, no params.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::SyncOut {}

/// An automation lane as a graph node: no controls, the beat read from its
/// block's `Env`.
impl<F: Float> SessionNode<F> for tutti_nodes::automation::AutomationLaneNode {}

/// A MIDI clip as a graph node: its `MidiClipControls`, no params; its
/// events are a published value (`clip.events()`), and the clips it replaced
/// are freed by its reclaimer.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::MidiClipNode {}

/// The MPE transform: its `MpeTransformControls` read its losses, no params;
/// its mode is the session's cell, which the session sweeps.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::MpeTransformNode {}

/// The harmony node: its `HarmonyControls`, no params; its changes are a
/// published value (`harmony.changes()`), and the lanes it replaced are freed
/// by its reclaimer.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::HarmonyNode {}

/// The MIDI input node: its `MidiInputControls`, no params; the Profile zones
/// it replaced freed by its reclaimer.
#[cfg(feature = "midi")]
impl<F: Float> SessionNode<F> for tutti_midi_runtime::MidiInputNode {}

/// The SoundFont player as a graph node.
#[cfg(feature = "soundfont")]
impl<F: Float + tutti_dsp::SimdLanes> SessionNode<F> for tutti_soundfont::SoundFontUnit<F> {}

// The two panners: bearing and height, and the VBAP spread and width or the
// HRTF blend, as a `ParamSet`.
param_session_node!(floats: VbapPannerNode, HrtfBinauralNode);

// The synth as a graph node: one MIDI event input, and its live params
// (master volume, unison detune and spread) as a `ParamSet`, so a param
// write reaches it and a fork of it starts from what was set.
#[cfg(feature = "synth")]
impl<F: tutti_polysynth::SynthFloat> SessionNode<F> for tutti_polysynth::PolySynth<F> {}

/// A voice as a graph node: its params (the gain) addressed by its set, so a
/// param write reaches the cell the node reads and a fork starts from the
/// gain as set; its window, loop, direction and speed are published values,
/// and the placements it replaced are freed by its reclaimer.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::VoiceNode<F> {}

/// A pool as a graph node; its controls are its `VoicePoolHandle`, whose
/// retired voices its reclaimer frees.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::VoicePool<F> {
    fn bind_timeline(self, timeline: &TimelineView) -> Self {
        self.loops(timeline.loops().clone())
    }
}

/// A bare clip reader as a graph node, its gain addressed by its set.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::MemorySource<F> {}

/// A [`Clip`](tutti_sampler::Clip) as a graph node: the voice node it
/// builds, its gain addressed by its set, and the placements it replaced (and
/// a streamed clip's stream) kept by its reclaimer for as long as the node is
/// in the graph.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::Clip<F> {}

/// A streamed voice as a graph node; its controls are its
/// `DiskVoiceControls`.
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> SessionNode<F> for tutti_sampler::DiskVoice<F> {}

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
