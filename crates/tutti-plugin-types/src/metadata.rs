//! Format-agnostic plugin load data.
//!
//! [`PluginWiring`] is the wiring data produced when a plugin is actually
//! instantiated (its buses, latency, tail, capability flags). It rides the
//! plugin-load IPC reply, so it is serializable under the `serde` feature.

use tutti_graph::BusRole;
use tutti_units::{SampleRate, Samples};

use crate::Measured;

use crate::{
    ChannelLayout, ChannelTopology, FeatureReport, Features, MidiAssignment, NoteName, OwnBypass,
    ParamGroup,
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Per-bus channel widths for one process direction, in bus-index order: the
/// widths alone, for sizing buffers (the shared-memory slab, a format's port
/// table). What a plugin *declares* about a bus is a [`PluginBus`].
pub type BusChannels = Vec<ChannelLayout>;

/// One audio bus a plugin declares: its width, where its channels sit, and what
/// it is for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PluginBus {
    /// The channel count, which sizes the buffers.
    pub channels: ChannelLayout,
    /// Which speaker each channel feeds, when the loader could name every one.
    ///
    /// `None` is "unknown": the plugin declined, the host has not asked (VST2, until an
    /// arrangement is negotiated), or the layout uses speakers this vocabulary does not
    /// yet name.
    /// It is never "no speakers": a bus with no channels is `Some` of an empty
    /// topology. A topology whose width is not [`channels`](Self::channels)'
    /// cannot be acted on; [`placement`](Self::placement) filters it out.
    pub topology: Option<ChannelTopology>,
    /// What the bus is for, as the format says it: [`BusRole::Main`] for the
    /// bus the format marks main (CLAP's `CLAP_AUDIO_PORT_IS_MAIN`, VST3's
    /// `kMain`, the one bus VST2 has, AU's element 0) and [`BusRole::Aux`] for
    /// the rest. No format marks a bus a sidechain, so no loader reports
    /// [`BusRole::Sidechain`].
    #[cfg_attr(feature = "serde", serde(with = "role_wire"))]
    pub role: BusRole,
}

impl PluginBus {
    /// A bus of `channels` with `role` and no placement.
    #[must_use]
    pub const fn new(channels: ChannelLayout, role: BusRole) -> Self {
        Self {
            channels,
            topology: None,
            role,
        }
    }

    /// A main bus of `channels` with no placement.
    #[must_use]
    pub const fn main(channels: ChannelLayout) -> Self {
        Self::new(channels, BusRole::Main)
    }

    /// The buses of a direction a format gives one bus (VST2, AU's element 0):
    /// a main bus of `channels`, or none for no channels (an instrument's
    /// inputs), since a list holds only buses the plugin has.
    #[must_use]
    pub fn single(channels: ChannelLayout) -> PluginBuses {
        if channels.count() == 0 {
            PluginBuses::new()
        } else {
            vec![Self::main(channels)]
        }
    }

    /// This bus with the placement `topology`.
    #[must_use]
    pub fn placed(self, topology: ChannelTopology) -> Self {
        Self {
            topology: Some(topology),
            ..self
        }
    }

    /// The bus's placement when it has one that agrees with its width: the
    /// one a caller routing by speaker may act on.
    #[must_use]
    pub fn placement(&self) -> Option<&ChannelTopology> {
        self.topology
            .as_ref()
            .filter(|t| t.width() == self.channels)
    }
}

/// The buses of one direction, in bus-index order. Empty is a direction with
/// no audio (an instrument's inputs): a loader lists every bus it has.
pub type PluginBuses = Vec<PluginBus>;

/// The total width of `buses`.
fn width(buses: &[PluginBus]) -> usize {
    buses.iter().map(|b| b.channels.count() as usize).sum()
}

/// The first direction of a plugin with `inputs` and `outputs` channels that is
/// past what a block serves.
fn check_widths(inputs: usize, outputs: usize) -> Result<(), crate::PluginTooWide> {
    for (direction, channels) in [("input", inputs), ("output", outputs)] {
        if channels > crate::MAX_BLOCK_CHANNELS {
            return Err(crate::PluginTooWide {
                direction,
                channels,
            });
        }
    }
    Ok(())
}

/// The widths of `buses`, in order.
fn widths(buses: &[PluginBus]) -> BusChannels {
    buses.iter().map(|b| b.channels).collect()
}

/// [`BusRole`] on the wire, as one byte: `tutti-graph` has no serde, and the
/// byte is the role's order in its declaration.
#[cfg(feature = "serde")]
mod role_wire {
    use super::BusRole;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(role: &BusRole, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(match role {
            BusRole::Main => 0,
            BusRole::Sidechain => 1,
            BusRole::Aux => 2,
        })
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BusRole, D::Error> {
        match u8::deserialize(d)? {
            0 => Ok(BusRole::Main),
            1 => Ok(BusRole::Sidechain),
            2 => Ok(BusRole::Aux),
            other => Err(D::Error::custom(format_args!("no bus role {other}"))),
        }
    }
}

/// What a plugin declares on its audio ports: the buses, per direction.
///
/// Two layouts are equal when every bus agrees, which is how a re-read after a
/// restart decides the host must be told.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Layout {
    /// The input buses.
    pub inputs: PluginBuses,
    /// The output buses.
    pub outputs: PluginBuses,
}

/// The most buses a [`Layout`] holds in a direction and still be believed: a
/// plugin has a handful, and a layout read off a wire is held to it.
pub const MAX_LAYOUT_BUSES: usize = 32;

impl Layout {
    /// Whether this is a layout a plugin can have: at most
    /// [`MAX_LAYOUT_BUSES`] buses in a direction and a width a block serves.
    /// What a layout that came off a wire passes before it is kept: an
    /// implausible `PluginIoChanged` layout is dropped and counted with the
    /// dropped events, and the host keeps the layout it held, so no
    /// `PortsRefused` follows.
    #[must_use]
    pub fn is_plausible(&self) -> bool {
        self.inputs.len() <= MAX_LAYOUT_BUSES
            && self.outputs.len() <= MAX_LAYOUT_BUSES
            && self.check_width().is_ok()
    }

    /// Whether every direction is within the width a block serves.
    ///
    /// # Errors
    ///
    /// [`PluginTooWide`](crate::PluginTooWide) for the first direction past
    /// [`MAX_BLOCK_CHANNELS`](crate::MAX_BLOCK_CHANNELS).
    pub fn check_width(&self) -> Result<(), crate::PluginTooWide> {
        check_widths(self.total_inputs(), self.total_outputs())
    }

    /// The total input channel width across all buses.
    #[must_use]
    pub fn total_inputs(&self) -> usize {
        width(&self.inputs)
    }

    /// The total output channel width across all buses.
    #[must_use]
    pub fn total_outputs(&self) -> usize {
        width(&self.outputs)
    }

    /// The input buses' widths, in order.
    #[must_use]
    pub fn input_widths(&self) -> BusChannels {
        widths(&self.inputs)
    }

    /// The output buses' widths, in order.
    #[must_use]
    pub fn output_widths(&self) -> BusChannels {
        widths(&self.outputs)
    }
}

use tutti_graph::MAX_NODE_TAIL;

/// How long a plugin keeps producing audio after its input goes silent.
///
/// An alias of the engine-wide [`Tail`](tutti_units::Tail), so a hosted
/// plugin's tail composes with any other node's. The format-specific decoding
/// (AU's seconds, the `u32::MAX` sentinel CLAP and VST3 share) is documented on
/// the constructor that performs it.
pub use tutti_units::Tail as PluginTail;

/// How a plugin processes: the tail it claims, and what a probe measured of
/// it ([`Measured`]). The one place the tail ceiling the graph parks a plugin
/// node on is decided ([`ceiling`](Self::ceiling)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Processing {
    tail: PluginTail,
    measured: Option<Measured>,
}

impl Processing {
    /// A plugin that claims `tail` and has not been measured.
    pub const fn new(tail: PluginTail) -> Self {
        Self {
            tail,
            measured: None,
        }
    }

    /// This claim with a probe's measurement beside it.
    #[must_use]
    pub const fn with_measured(mut self, measured: Measured) -> Self {
        self.measured = Some(measured);
        self
    }

    /// The tail the plugin claims, as it reported it.
    pub const fn claim(&self) -> PluginTail {
        self.tail
    }

    /// What was measured, if anything was.
    pub const fn measured(&self) -> Option<&Measured> {
        self.measured.as_ref()
    }

    /// The ceiling `Shape::tail` gets. **A measurement raises a finite
    /// claim and never lowers or invents one**: `None`, `Unknown` and
    /// `Unbounded` stand whatever was measured (one impulse at default
    /// settings proves nothing about another patch), and a finite claim is
    /// never cut below what the plugin said.
    ///
    /// **A finite figure past [`MAX_NODE_TAIL`] at `rate` is `Unbounded`**, never
    /// that figure clamped: `rate` is the graph's (the plugin node's prepared
    /// rate), at which the graph bounds a node's finite tail (it sizes a
    /// bounce's file from it), and a ceiling cut to 60 s would end a two-minute
    /// reverb early and park its node while it still rings. `Unbounded` claims
    /// nothing false: nothing parks on it and an export runs to its own silence
    /// or range rule.
    pub fn ceiling(&self, rate: SampleRate) -> PluginTail {
        let raised = match (self.tail, self.measured.and_then(|m| m.tail)) {
            (PluginTail::Finite(claimed), Some(measured)) if measured > claimed => {
                PluginTail::Finite(measured)
            }
            (tail, _) => tail,
        };
        match raised {
            PluginTail::Finite(frames) if frames > MAX_NODE_TAIL.to_samples_ceil(rate) => {
                PluginTail::Unbounded
            }
            other => other,
        }
    }
}

/// The wiring data a plugin reports when it is instantiated.
///
/// Produced by the plugin server at load time and returned over IPC; never
/// persisted (unlike the catalog [`PluginDescriptor`](crate::PluginDescriptor),
/// which is). Carries what the audio graph needs to wire the node: the buses
/// and placement, latency, tail, and the [`Features`] capability flags.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PluginWiring {
    /// The input buses, in bus-index order; see [`PluginBuses`].
    pub inputs: PluginBuses,
    /// The output buses.
    pub outputs: PluginBuses,
    /// Reported initial processing latency in sample frames, used for plugin
    /// delay compensation. [`latency`](Self::latency) reads whether it is
    /// non-zero.
    pub latency_samples: Samples,
    /// How long the plugin keeps sounding after its input stops — see
    /// [`PluginTail`].
    ///
    /// Sits beside `latency_samples` because both are engine-wiring numbers a
    /// render needs, but it is a sum type rather than a count: "unbounded" and
    /// "never asked" are real answers here, and neither is a number.
    ///
    /// Deserializes as [`PluginTail::Unknown`] when absent.
    #[cfg_attr(feature = "serde", serde(default))]
    pub tail: PluginTail,
    /// Capability flags the plugin reported at load, such as
    /// [`Features::F64_AUDIO`] and [`Features::MIDI_IN`].
    ///
    /// Read through [`capability`](Self::capability) when the answer feeds a
    /// person — a clear bit here is also what an unprobed capability looks like,
    /// and [`probed`](Self::probed) is the half that tells them apart.
    /// The per-block send-gate reads this field directly and should: it has no
    /// way to act on "unknown" and must stay one mask-and-compare.
    pub features: Features,
    /// Which capabilities the loader actually probed.
    ///
    /// Loaders probe different subsets (see
    /// [`features::probed`](crate::features::probed)). Without this mask, a
    /// plugin that was asked about MIDI and declined looks the same as one
    /// nobody asked. Empty ("nothing was asked") when absent from a serialized
    /// value.
    // `serde(default)` covers JSON and struct-update construction; a short
    // postcard payload is `PROTOCOL_VERSION`'s job, not serde's.
    #[cfg_attr(feature = "serde", serde(default))]
    pub probed: Features,
    /// The plugin's bypass of its own, where it keeps it: VST3's `kIsBypass`
    /// parameter, CLAP's `CLAP_PARAM_IS_BYPASS` parameter, AU's
    /// `kAudioUnitProperty_BypassEffect`, VST2's `effSetBypass` (only where the
    /// plugin accepts it). `None` for a plugin without one, or one that refuses.
    /// A node over it declares
    /// [`Shape::own_bypass`](tutti_graph::Shape::own_bypass) and the
    /// host writes the plugin's bypass when the node's mode changes
    /// ([`Control::set_bypassed`](crate::Control::set_bypassed)).
    #[cfg_attr(feature = "serde", serde(default))]
    pub own_bypass: Option<OwnBypass>,
    /// The controller table the plugin declares (VST3 `IMidiMapping`, AU
    /// `kAudioUnitProperty_AllParameterMIDIMappings`): empty for a format or a
    /// plugin with none. VST3's host applies it (it turns each mapped controller
    /// into a parameter change) and an AU applies its own table inside the unit;
    /// [`Control::rebuild_midi_mapping`](crate::Control::rebuild_midi_mapping)
    /// re-reads it, and [`Control::wiring`](crate::Control::wiring) answers the
    /// table read last.
    #[cfg_attr(feature = "serde", serde(default))]
    pub assignments: Vec<MidiAssignment>,
    /// Who applies [`assignments`](Self::assignments): the host's node
    /// ([`Host`](crate::MidiMapApplied::Host): VST3, whose host must, and the
    /// formats that declare none) or the plugin itself
    /// ([`Plugin`](crate::MidiMapApplied::Plugin): an AU's unit routes its own
    /// table). Known at load, so a node translates the declared table from its
    /// first block with no call to the plugin.
    ///
    /// A wiring written before the field existed reads as
    /// [`Host`](crate::MidiMapApplied::Host).
    #[cfg_attr(feature = "serde", serde(default = "applied_by_the_host"))]
    pub assignments_applied: crate::MidiMapApplied,
    /// The names the plugin gives to MIDI keys (CLAP `note-name`, VST3
    /// `IKeyswitchController`, VST2 `effGetMidiKeyName`): empty for a plugin
    /// with none. The plugin's programs are
    /// [`Control::presets`](crate::Control::presets), not here.
    #[cfg_attr(feature = "serde", serde(default))]
    pub note_names: Vec<NoteName>,
    /// The groups the plugin sorts its parameters into, parents before
    /// children: empty for a format or a plugin with no tree.
    #[cfg_attr(feature = "serde", serde(default))]
    pub param_groups: Vec<ParamGroup>,
}

/// What a wiring written before `assignments_applied` existed says: the host
/// applies the declared table.
#[cfg(feature = "serde")]
fn applied_by_the_host() -> crate::MidiMapApplied {
    crate::MidiMapApplied::Host
}

/// A wiring of no buses and nothing declared; the host applies the (empty)
/// declared table.
impl Default for PluginWiring {
    fn default() -> Self {
        Self {
            inputs: PluginBuses::default(),
            outputs: PluginBuses::default(),
            latency_samples: Samples::default(),
            tail: PluginTail::default(),
            features: Features::default(),
            probed: Features::default(),
            own_bypass: None,
            assignments: Vec::new(),
            assignments_applied: crate::MidiMapApplied::Host,
            note_names: Vec::new(),
            param_groups: Vec::new(),
        }
    }
}

impl PluginWiring {
    /// Whether every direction is within the width a block serves.
    ///
    /// # Errors
    ///
    /// [`PluginTooWide`](crate::PluginTooWide) for the first direction past
    /// [`MAX_BLOCK_CHANNELS`](crate::MAX_BLOCK_CHANNELS).
    pub fn check_width(&self) -> Result<(), crate::PluginTooWide> {
        check_widths(self.total_inputs(), self.total_outputs())
    }

    /// Returns the total input channel width across all buses.
    pub fn total_inputs(&self) -> usize {
        width(&self.inputs)
    }

    /// Returns the total output channel width across all buses.
    pub fn total_outputs(&self) -> usize {
        width(&self.outputs)
    }

    /// The input buses' widths, in order: what sizes the input buffers.
    #[must_use]
    pub fn input_widths(&self) -> BusChannels {
        widths(&self.inputs)
    }

    /// The output buses' widths, in order.
    #[must_use]
    pub fn output_widths(&self) -> BusChannels {
        widths(&self.outputs)
    }

    /// Returns `true` if every bus in both directions has a
    /// [`placement`](PluginBus::placement): the consistency a caller routing
    /// by speaker depends on. False when any bus's placement is unknown or
    /// disagrees with its width, which is not an error, just not enough
    /// information.
    pub(crate) fn topology_is_complete(&self) -> bool {
        self.inputs
            .iter()
            .chain(&self.outputs)
            .all(|b| b.placement().is_some())
    }

    /// Returns `true` if the plugin reports non-zero processing latency, so it
    /// takes part in plugin delay compensation.
    pub fn latency(&self) -> bool {
        self.latency_samples > Samples::ZERO
    }

    /// Returns `Some(answer)` when the loader probed `f`, [`None`] when it did
    /// not.
    ///
    /// Pass exactly one bit.
    ///
    /// The read for anything user-facing. The send-gate uses `features`
    /// directly — see the field docs for why the two differ.
    pub fn capability(&self, f: Features) -> Option<bool> {
        self.report().get(f)
    }

    /// Returns [`features`](Self::features) and [`probed`](Self::probed) as one
    /// [`FeatureReport`].
    pub fn report(&self) -> FeatureReport {
        FeatureReport::new(self.probed, self.features)
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    use crate::{LatencyVerdict, PROBE_FLOOR, ProbeConfig, StateHash, TailVerdict, Verdict};
    use tutti_buffers::MaxBlock;
    use tutti_units::SampleRate;

    fn stereo() -> ChannelTopology {
        ChannelTopology::default_for(ChannelLayout::STEREO)
    }

    /// A stereo main input with its placement, a mono aux with none.
    fn main_and_aux() -> PluginBuses {
        vec![
            PluginBus::new(ChannelLayout::STEREO, BusRole::Main).placed(stereo()),
            PluginBus::new(ChannelLayout::MONO, BusRole::Aux),
        ]
    }

    /// A multi-bus plugin round-trips through the postcard wire, every bus's
    /// width, placement and role with it, and the totals sum across buses.
    ///
    /// The `None` placement is load-bearing: "unknown" is a different answer
    /// from `Some(empty)` ("no channels"), and a round trip that collapsed the
    /// two would let a caller read an aux bus's speakers it never reported.
    #[test]
    fn a_multi_bus_plugin_round_trips() {
        let loaded = PluginWiring {
            inputs: main_and_aux(),
            outputs: vec![PluginBus::new(ChannelLayout::STEREO, BusRole::Main)],
            latency_samples: Samples::new(128),
            tail: PluginTail::Finite(Samples::new(48_000)),
            features: Features::F64_AUDIO | Features::MIDI_IN,
            probed: Features::F64_AUDIO | Features::MIDI_IN | Features::EDITOR,
            own_bypass: Some(OwnBypass::Parameter(tutti_units::ParamId::new(4))),
            assignments: vec![MidiAssignment {
                channel: tutti_ump::MidiChannel::new(3),
                source: crate::MidiSource::PitchBend,
                param: tutti_units::ParamId::new(9),
            }],
            assignments_applied: crate::MidiMapApplied::Plugin,
            note_names: vec![NoteName {
                key: tutti_ump::NoteNumber::of(36),
                channel: None,
                text: "Kick".into(),
            }],
            param_groups: vec![ParamGroup {
                path: "Filter".into(),
                label: "Filter".into(),
                parent: None,
            }],
        };
        let bytes = postcard::to_allocvec(&loaded).unwrap();
        let back: PluginWiring = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, loaded);
        assert_eq!(back.inputs[1].topology, None);
        assert_eq!(back.total_inputs(), 3);
        assert_eq!(back.total_outputs(), 2);
        assert_eq!(
            back.input_widths(),
            vec![ChannelLayout::STEREO, ChannelLayout::MONO]
        );
        assert!(back.latency());
    }

    /// **Every role crosses the wire as itself**, and a byte that names no
    /// role is refused rather than read as one.
    #[test]
    fn every_role_crosses_the_wire_and_an_unknown_one_is_refused() {
        for role in [BusRole::Main, BusRole::Sidechain, BusRole::Aux] {
            let bus = PluginBus::new(ChannelLayout::MONO, role);
            let bytes = postcard::to_allocvec(&bus).unwrap();
            assert_eq!(postcard::from_bytes::<PluginBus>(&bytes).unwrap(), bus);
        }
        let mut bytes =
            postcard::to_allocvec(&PluginBus::new(ChannelLayout::MONO, BusRole::Aux)).unwrap();
        *bytes.last_mut().unwrap() = 3;
        assert!(postcard::from_bytes::<PluginBus>(&bytes).is_err());
    }

    /// An unknown bus makes the topology incomplete, and a full one completes it.
    ///
    /// `topology_is_complete` is what a caller checks before routing by
    /// speaker, so both answers are pinned — a predicate hardwired to `false`
    /// would satisfy the negative half alone.
    #[test]
    fn topology_is_complete_only_when_every_bus_agrees_with_its_count() {
        let mono = ChannelTopology::default_for(ChannelLayout::MONO);
        let bus = |topology: Option<ChannelTopology>| {
            let bus = PluginBus::new(ChannelLayout::STEREO, BusRole::Main);
            match topology {
                Some(topology) => bus.placed(topology),
                None => bus,
            }
        };
        let of = |input, output| PluginWiring {
            inputs: vec![bus(input)],
            outputs: vec![bus(output)],
            ..Default::default()
        };
        assert!(of(Some(stereo()), Some(stereo())).topology_is_complete());
        assert!(
            !of(None, Some(stereo())).topology_is_complete(),
            "an unknown bus is not complete"
        );
        // The count sizes the buffer, so a topology of another width describes
        // another bus.
        assert!(
            !of(Some(mono), Some(stereo())).topology_is_complete(),
            "a mono topology on a stereo bus describes a different bus"
        );
    }

    /// The probed mask crosses the wire, so an unprobed capability stays
    /// distinguishable from a declined one on the receiving side.
    #[test]
    fn the_probed_mask_crosses_the_wire() {
        // A report with one capability probed.
        let loaded = PluginWiring {
            features: Features::EDITOR,
            probed: Features::EDITOR,
            ..Default::default()
        };
        let bytes = postcard::to_allocvec(&loaded).unwrap();
        let back: PluginWiring = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(back.capability(Features::EDITOR), Some(true));
        assert_eq!(
            back.capability(Features::MIDI_IN),
            None,
            "a capability the loader never probed must not arrive as a false"
        );
    }

    /// A default `PluginWiring` claims nothing, rather than claiming that every
    /// capability was probed and declined.
    #[test]
    fn a_defaulted_plugin_claims_no_capability() {
        let loaded = PluginWiring::default();
        assert_eq!(loaded.capability(Features::EDITOR), None);
        assert_eq!(loaded.capability(Features::TRANSPORT), None);
    }

    /// A plugin with no buses round-trips and has no width.
    #[test]
    fn empty_buses_round_trip() {
        let loaded = PluginWiring::default();
        let bytes = postcard::to_allocvec(&loaded).unwrap();
        let back: PluginWiring = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, loaded);
        assert!(back.inputs.is_empty());
        assert_eq!(back.total_inputs(), 0);
        assert_eq!(back.features, Features::empty());
        assert!(!back.latency());
    }

    /// Every tail arm survives the wire distinctly.
    ///
    /// The value semantics are pinned where the type lives; what belongs here is
    /// the *encoding*, because this is the crate whose postcard IPC carries it.
    /// `PluginTail` is an alias rather than a local copy, and an alias changes
    /// no discriminant, no arm order and no payload — this test is the evidence
    /// for that, so it must keep passing unedited.
    #[test]
    fn every_tail_arm_survives_the_wire() {
        for tail in [
            PluginTail::Unknown,
            PluginTail::None,
            PluginTail::Finite(Samples::new(48_000)),
            PluginTail::Unbounded,
        ] {
            let bytes = postcard::to_allocvec(&tail).unwrap();
            assert_eq!(postcard::from_bytes::<PluginTail>(&bytes).unwrap(), tail);
        }
    }

    fn config() -> ProbeConfig {
        ProbeConfig {
            rate: SampleRate::SR_48K,
            block: MaxBlock::new(Samples::new(512)).unwrap(),
            floor: PROBE_FLOOR,
            state: StateHash::of(&[]),
        }
    }

    fn measured_with(tail: Option<usize>) -> Measured {
        Measured {
            tail: tail.map(Samples::new),
            latency: None,
            config: config(),
            verdict: Verdict {
                tail: TailVerdict::Honest,
                latency: LatencyVerdict::Honest,
            },
        }
    }

    fn measured(tail: usize) -> Measured {
        measured_with(Some(tail))
    }

    /// **A measured tail above a finite claim raises the ceiling to it**: a
    /// plugin that under-reports is not parked before it stops ringing.
    #[test]
    fn a_longer_measurement_raises_a_finite_claim() {
        let p = Processing::new(PluginTail::Finite(Samples::new(100))).with_measured(measured(250));
        assert_eq!(
            p.ceiling(SampleRate::SR_48K),
            PluginTail::Finite(Samples::new(250))
        );
    }

    /// **A shorter measurement never lowers the claim**, and an equal one
    /// leaves it: the ceiling is the plugin's word at least.
    #[test]
    fn a_shorter_measurement_does_not_lower_the_claim() {
        for m in [40, 100] {
            let p =
                Processing::new(PluginTail::Finite(Samples::new(100))).with_measured(measured(m));
            assert_eq!(
                p.ceiling(SampleRate::SR_48K),
                PluginTail::Finite(Samples::new(100)),
                "measured {m}"
            );
        }
    }

    /// **`None`, `Unknown` and `Unbounded` stand whatever was measured**: a
    /// measurement invents no tail for a plugin that claims none, and bounds
    /// none that is unbounded or unasked.
    #[test]
    fn a_measurement_does_not_invent_or_bound_a_tail() {
        for claim in [PluginTail::None, PluginTail::Unknown, PluginTail::Unbounded] {
            let p = Processing::new(claim).with_measured(measured(250));
            assert_eq!(p.ceiling(SampleRate::SR_48K), claim);
        }
    }

    /// Unmeasured, the ceiling is the claim: what every plugin is until the
    /// probe has measured it (an empty measurement included).
    #[test]
    fn an_unmeasured_plugin_is_its_claim() {
        for claim in [
            PluginTail::None,
            PluginTail::Unknown,
            PluginTail::Unbounded,
            PluginTail::Finite(Samples::new(7)),
        ] {
            assert_eq!(Processing::new(claim).ceiling(SampleRate::SR_48K), claim);
            let empty = Processing::new(claim).with_measured(measured_with(None));
            assert_eq!(empty.ceiling(SampleRate::SR_48K), claim);
        }
    }

    /// **A finite tail past the graph's bound is `Unbounded`, not clamped**: a
    /// clamp would declare a ceiling shorter than the plugin rings; the bound
    /// itself still fits. The bound is sixty seconds at the plugin node's
    /// rate: 2 880 000 frames at 48 kHz and 5 760 000 at 96 kHz.
    #[test]
    fn a_finite_tail_past_the_graphs_bound_is_unbounded() {
        for (rate, bound) in [
            (SampleRate::SR_48K, 2_880_000),
            (SampleRate::SR_96K, 5_760_000),
        ] {
            assert_eq!(MAX_NODE_TAIL.to_samples_ceil(rate), Samples::new(bound));
            let at = PluginTail::Finite(Samples::new(bound));
            assert_eq!(Processing::new(at).ceiling(rate), at, "{rate:?}");
            let past = PluginTail::Finite(Samples::new(bound + 1));
            assert_eq!(
                Processing::new(past).ceiling(rate),
                PluginTail::Unbounded,
                "{rate:?}"
            );
            // A measurement that raises a claim past the bound goes the same way.
            let raised = Processing::new(PluginTail::Finite(Samples::new(10)))
                .with_measured(measured(bound + 1));
            assert_eq!(raised.ceiling(rate), PluginTail::Unbounded, "{rate:?}");
        }
    }

    /// The same finite figure is judged at the rate of the graph it sits in:
    /// 4 000 000 frames is 83 s at 48 kHz, unbounded, and 41.7 s at 96 kHz, a
    /// figure.
    #[test]
    fn a_finite_tail_is_judged_in_seconds_at_the_nodes_rate() {
        let tail = PluginTail::Finite(Samples::new(4_000_000));
        assert_eq!(
            Processing::new(tail).ceiling(SampleRate::SR_48K),
            PluginTail::Unbounded
        );
        assert_eq!(Processing::new(tail).ceiling(SampleRate::SR_96K), tail);
    }
}
