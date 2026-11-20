//! The VST2-specific value types of the host, [`PluginInfo`] and [`Vst2ProcessContext`].

use tutti_graph::UmpAt;
use tutti_plugin_types::SampleRate;
use tutti_plugin_types::{PluginVersion, TransportInfo, Vst2Category};

/// What the plugin declared at load time; see [`Vst2Loaded::metadata`](crate::Vst2Loaded::metadata).
///
/// The figures a plugin changes after load (its pin counts, latency and tail)
/// are not here: [`Vst2Loaded::pins`](crate::Vst2Loaded::pins),
/// [`latency_samples`](crate::Vst2Loaded::latency_samples) and
/// [`tail`](crate::Vst2Loaded::tail) read them from the plugin when asked.
///
/// VST2 encodes the tail inversely to every other format, which is why
/// [`Vst2Loaded::tail`](crate::Vst2Loaded::tail) decodes it here rather than
/// through [`PluginTail::from_vst3_samples`](tutti_plugin_types::PluginTail::from_vst3_samples): on the wire `0` means "no tail
/// *information*, host decides" and `1` means "no tail at all".
/// `from_vst3_samples` maps `0 => None`, so feeding it a VST2 answer reads
/// "unknown" as "silent" — and a bounce sizing its render from that adds no
/// decay, truncating every reverb.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// The plugin's `unique_id`, as the unsigned number a [`PluginId::Vst2`](tutti_plugin_types::PluginId) holds.
    pub id: u32,
    /// Display name from `effGetEffectName`, or the file stem if the plugin
    /// declares none.
    pub name: String,
    /// Vendor string from `effGetVendorString`; empty if the plugin declines.
    pub vendor: String,
    /// `AEffect::version`, a bare `i32` the spec gives no encoding, kept as the integer
    /// ([`PluginVersion::Integer`]) rather than decoded into parts.
    pub version: PluginVersion,
    /// The plugin's declared VST2 category, carried verbatim. Callers classify
    /// it themselves rather than relying on the derived `receives_midi` flag.
    pub category: Vst2Category,
    /// `true` if the plugin is a synth or declares MIDI input/output.
    pub receives_midi: bool,
    /// `true` if the plugin declares at least one MIDI **output** bus
    /// (`get_info().midi_outputs > 0`) — it emits MIDI the host reads back.
    pub emits_midi: bool,
    /// `true` if the plugin declared `effFlagsHasEditor`. A `false` here means
    /// [`crate::Vst2Loaded`]'s editor calls have nothing to open.
    pub has_editor: bool,
    /// `true` if the plugin declared `effFlagsCanDoubleReplacing`.
    ///
    /// Load-bearing, not informational: an activation told 64-bit tells the
    /// plugin `effSetProcessPrecision(64)` only when this is set, and the
    /// Processor reads it to choose between `processReplacingF64` and the
    /// narrowing f32 fallback.
    pub supports_f64: bool,
}

/// Per-block inputs to [`Vst2Processor::process_planes`] beyond the audio buffers.
///
/// [`Vst2Processor::process_planes`]: crate::Vst2Processor::process_planes
#[derive(Debug)]
pub struct Vst2ProcessContext<'a> {
    /// Events to deliver before the block renders, each carrying its own frame
    /// offset within the block. Empty is normal for an effect.
    pub midi: &'a [UmpAt],
    /// Transport state for this block. `None` makes the plugin's `VstTimeInfo`
    /// report a stopped transport at the origin ([`TransportInfo::new`]), which is what a
    /// plugin syncing to host tempo will see.
    pub transport: Option<&'a TransportInfo>,
    /// The sample rate, reported to the plugin in its `VstTimeInfo` when
    /// [`transport`](Self::transport) is set. It does not reconfigure the
    /// plugin: the rate it runs at is the activation's
    /// ([`Prepare::rate`](tutti_plugin_types::Prepare::rate)).
    pub sample_rate: SampleRate,
}

impl Default for Vst2ProcessContext<'_> {
    fn default() -> Self {
        Self::new(SampleRate::DEFAULT)
    }
}

impl<'a> Vst2ProcessContext<'a> {
    /// Creates a context with no MIDI and no transport, carrying only
    /// `sample_rate` in Hz. Layer the rest on with [`Self::midi`] and
    /// [`Self::transport`].
    #[must_use]
    pub fn new(sample_rate: SampleRate) -> Self {
        Self {
            midi: &[],
            transport: None,
            sample_rate,
        }
    }

    /// Attaches this block's MIDI events, replacing any already set.
    #[must_use]
    pub fn midi(mut self, midi: &'a [UmpAt]) -> Self {
        self.midi = midi;
        self
    }

    /// Attaches this block's transport state, replacing any already set.
    #[must_use]
    pub fn transport(mut self, transport: &'a TransportInfo) -> Self {
        self.transport = Some(transport);
        self
    }
}
