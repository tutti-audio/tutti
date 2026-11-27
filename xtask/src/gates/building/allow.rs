//! The rows of types of another kind: the violations of each row are not counted.

use super::scan::Site;
use super::{Rule, crate_of};

/// A type of another kind: the violations of `rules` (all when empty) of items
/// under `path`, owned by `owner` (any when empty; `Type::fn` names one function), are
/// not counted. A row that covers
/// no violation fails the gate.
pub(in crate::gates) struct Allow {
    /// A repo path prefix: a crate's directory, or a file.
    pub(super) path: &'static str,
    pub(super) owner: &'static str,
    pub(super) rules: &'static [Rule],
    pub(super) reason: &'static str,
}

/// The path of every `*-ffi` crate.
pub(super) const FFI: &str = "crates/*-ffi/";

const GRAPH: &str = "crates/tutti-graph/";

pub(super) const CTOR: &[Rule] = &[Rule::CtorResult];
const RANGE: &[Rule] = &[Rule::RangeMacro];
const SETTER: &[Rule] = &[Rule::Setter];
pub(super) const WIDTH: &[Rule] = &[Rule::ConfigWidth];
const DEFAULT_ENUM: &[Rule] = &[Rule::DefaultEnumArg];
pub(super) const FIELDS: &[Rule] = &[Rule::PubFieldConfig];

/// An allow row for one type `owner` under `path`, for `rules`.
pub(super) const fn ty(
    path: &'static str,
    owner: &'static str,
    rules: &'static [Rule],
    reason: &'static str,
) -> Allow {
    Allow {
        path,
        owner,
        rules,
        reason,
    }
}

const COPY_WITH_ONE_FIELD: &str = "a copy with one field replaced (std's `with_` as in \
    `Path::with_extension`), whose getter holds the bare name";

const STEREO_ONLY: &str = "a stereo-only node: mid/side, width and auto-pan are defined on a \
    left and right pair, so the width is not a choice and the config takes none";
const MONO_ONLY: &str = "a mono-only node: an EQ band is one filter section on one channel, so \
    the width is not a choice and the config takes none";
const STEREO_PLATE: &str = "a stereo-only node: the plate's two tank halves feed the left and \
    right ears, so the width is not a choice and the config takes none";
const FIXED_SOURCE: &str = "a source with no audio input and one fixed output (mono, or the \
    metronome's stereo pair): the width is not a choice, so the config takes none";
const NO_AUDIO_PORT: &str = "a node with no audio port: it reads a curve, a program or MIDI and \
    sends or renders one value, so there is no width to choose";
const KERNEL_WIDTH: &str = "a kernel's width is its own: the buses its `shape` declares, so the \
    config takes none";
const MONO_IN_STEREO_OUT: &str = "a reverb with a fixed shape, mono in and stereo out: its width \
    is not a choice and the config takes none";
const OVER_A_LAYOUT: &str = "a node over a checked dsp layout: the layout's bus is the width, \
    and the config takes the layout";
const TEST_SUPPORT: &str =
    "test support behind the `testing` or `contract` feature, not an app's type";

/// The allow-list. A row is permanent policy: it covers the violations it names and
/// stays when none exist.
pub(in crate::gates) const ALLOW: &[Allow] = &[
    Allow {
        path: GRAPH,
        owner: "Row",
        rules: &[],
        reason: "the contract harness a node crate writes rows with: node-author test support",
    },
    Allow {
        path: GRAPH,
        owner: "ImpulseSource",
        rules: &[],
        reason: "the contract harness a node crate writes rows with: node-author test support",
    },
    Allow {
        path: "crates/tutti-graph/src/testing.rs",
        owner: "",
        rules: &[],
        reason: "tutti-graph's test nodes, for node authors' tests",
    },
    Allow {
        path: "crates/tutti-nodes/src/testing.rs",
        owner: "",
        rules: &[],
        reason: "tutti-nodes' test oscillator, for node authors' tests",
    },
    Allow {
        path: FFI,
        owner: "",
        rules: &[],
        reason: "an FFI crate's raw types mirror a C ABI or an OS call, not an app's configuration",
    },
    ty(
        "crates/tutti-dsp/src/decorrelator.rs",
        "VelvetNoise::new",
        CTOR,
        "a density and a rate must agree (at most one impulse a frame): validity of two arguments together, which neither type can carry",
    ),
    ty(
        "crates/tutti-dsp/src/decorrelator.rs",
        "Decorrelator::new",
        CTOR,
        "builds `VelvetNoise::new` per channel: the same density-against-rate validity",
    ),
    ty(
        "crates/tutti-buffers/src/input_output.rs",
        "InputOutput::new",
        CTOR,
        "two views' frame counts must agree: validity of two arguments together, which neither type can carry",
    ),
    ty(
        "crates/tutti-dsp/src/dct.rs",
        "Dct2::new",
        CTOR,
        "the coefficients kept are valid only against the points: at most the transform's input size, validity of two arguments together",
    ),
    ty(
        "crates/tutti-dsp/src/mfcc.rs",
        "Mfcc::new",
        CTOR,
        "the coefficients kept are valid only against the bank: at most its band count, validity of two arguments together",
    ),
    ty(
        "crates/tutti-dsp/src/dynamics/transient_detector.rs",
        "TransientTimes::new",
        CTOR,
        "the fast time is valid only below the slow one: validity of two arguments together, which neither type can carry",
    ),
    ty(
        "crates/tutti-plugin-types/src/channels.rs",
        "ChannelTable::of_planes",
        CTOR,
        "a table has a fixed number of slots: the width of a block is a runtime fact the slots must hold",
    ),
    ty(
        "crates/tutti-buffers/src/input_output.rs",
        "InputOutput::paired",
        CTOR,
        "the same validity as `InputOutput::new` with the frame count taken from the views: they must agree",
    ),
    ty(
        "crates/tutti-buffers/src/input_output.rs",
        "InputOutput::from_slices",
        CTOR,
        "the same validity as `InputOutput::new` over two slice tables: their frame counts must agree",
    ),
    ty(
        "crates/tutti-plugin-types/src/channels.rs",
        "ChannelTable::of_planes_mut",
        CTOR,
        "a table has a fixed number of slots: the width of a block is a runtime fact the slots must hold",
    ),
    Allow {
        path: "crates/tutti-midi-device/src/endpoint.rs",
        owner: "FunctionBlock::from_info",
        rules: &[Rule::CtorResult],
        reason: "a conversion from a Function Block Info Notification and a name, not a configuration: it refuses what no block can hold, with a named error",
    },
    ty(
        "crates/tutti-vst2-host/",
        "Vst2Loaded::load",
        CTOR,
        "loading a VST2 library the OS refuses: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-vst2-host/",
        "Vst2Loaded::load_plugin",
        CTOR,
        "loading a VST2 library the OS refuses: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-vst3-host/",
        "Vst3Loaded::load",
        CTOR,
        "loading a VST3 module the OS refuses: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-runtime/src/schedule/harmony_node.rs",
        "HarmonyChanges::new",
        CTOR,
        "the changes are valid together (no chord, and no scale, shares a beat with another): no single change's type holds it",
    ),
    ty(
        "crates/tutti-nodes/src/dynamics/leveller.rs",
        "LevellerConfig::new",
        CTOR,
        "the bus is valid as a whole (an ambisonic bus, and speakers that are not a bed's, have no weights): a `ChannelTopology` holds any bus",
    ),
    ty(
        "crates/tutti-nodes/src/early_reflections.rs",
        "EarlyReflectionsConfig::new",
        CTOR,
        "the taps are valid against the limits of the stage realized from them (a finite gain, a channel a stage has, a delay a ring holds): a `TapSet` holds any tap",
    ),
    ty(
        "crates/tutti-nodes/src/filter/crossover.rs",
        "CrossoverConfig::new",
        CTOR,
        "the bus and the splits are valid together (`bands * channels` within the graph's ports): no single value's type holds it",
    ),
    ty(
        "crates/tutti-nodes/src/modulation/phaser.rs",
        "PhaserConfig::new",
        CTOR,
        "a stagger that names an offset for each channel is valid for the bus it names them for: no single value's type holds it",
    ),
    ty(
        "crates/tutti-dsp/src/spatial/hrtf/sphere.rs",
        "HrirSphere::new",
        CTOR,
        "the measurements are valid together (four or more distinct directions that enclose the listener, responses of one length): no single value's type holds it",
    ),
    ty(
        "crates/tutti-dsp/src/spatial/hrtf/sphere.rs",
        "HrirSphere::from_bytes",
        CTOR,
        "decoding a sphere's bytes: the failure is the bytes', not a value's",
    ),
    ty(
        "crates/tutti-sf2/",
        "SoundFont::open",
        CTOR,
        "opening a SoundFont source: the failure is the source's, not a value's",
    ),
    ty(
        "crates/tutti-rt/src/bells.rs",
        "Bells::new",
        CTOR,
        "starting the tick thread: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-network/src/udp.rs",
        "NetworkLink::spawn",
        CTOR,
        "duplicating the socket and starting the task's thread: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-network/src/udp.rs",
        "NetworkLink::bind",
        CTOR,
        "binding a UDP socket: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-au-host/",
        "AuHandle::new",
        CTOR,
        "loading an AU component the OS refuses: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-au-host/",
        "AuLoaded::new",
        CTOR,
        "instantiating an AU component the OS refuses: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-au-host/",
        "AuLoaded::new_with_config",
        CTOR,
        "instantiating an AU component the OS refuses: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-clap-host/",
        "ClapLoaded::load",
        CTOR,
        "loading a CLAP bundle the OS refuses: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-codec/",
        "WaveDecode::open",
        CTOR,
        "opening a file for a stepped decode: the failure is the file's, not a value's",
    ),
    ty(
        "crates/tutti-codec/",
        "FileIn::open",
        CTOR,
        "opening a file for decoding: the failure is the file's, not a value's",
    ),
    ty(
        "crates/tutti-codec/src/chain.rs",
        "Chain::new",
        CTOR,
        "beginning a file for the pipeline: the failure is the file's and the format's (an existing path, a length the format cannot hold), not a value's",
    ),
    ty(
        "crates/tutti-tempo/",
        "TempoMap::from_beats",
        CTOR,
        "a conversion from measured beats that can fail on the input: the beats' count and order are the caller's, not a value's",
    ),
    ty(
        "crates/tutti-platform/",
        "FileStamp::of",
        CTOR,
        "stamping a file: the failure is the file's metadata, not a value's",
    ),
    ty(
        "crates/tutti-dsp/",
        "Polarity::new",
        CTOR,
        "a channel is valid only against the width: the validity depends on the width and the channels together, T5's stated exception",
    ),
    ty(
        "crates/tutti-cpal/",
        "DeviceHost::open",
        CTOR,
        "opening an audio host: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-cpal/",
        "AudioEngine::new",
        CTOR,
        "opening an output device: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-cpal/",
        "AudioEngine::open",
        CTOR,
        "opening an output device: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-hardware/",
        "Devices::open",
        CTOR,
        "opening the OS MIDI endpoints: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-hardware/",
        "Devices::open_over",
        CTOR,
        "opening the OS MIDI endpoints: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-hardware/",
        "MidiSession::open",
        CTOR,
        "opening the OS MIDI ports: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-hardware/",
        "MidiSession::open_over",
        CTOR,
        "opening the OS MIDI ports: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-network/",
        "Browser::start",
        CTOR,
        "binding the mDNS browser: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-network/",
        "Nonce::random",
        CTOR,
        "reading OS entropy: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-midi-ci/src/pe/resource/fields.rs",
        "",
        RANGE,
        "text_enum! writes a closed enum keyed by its spec text, not a number range; `new(text)` returns the variant or `None`",
    ),
    ty(
        "crates/tutti-midi-ci/",
        "PeData::decode",
        CTOR,
        "a parse that needs the Resource it is read as, a second input a `TryFrom` has no place for",
    ),
    ty(
        "crates/tutti-midi-ci/",
        "PeData::decode_bytes",
        CTOR,
        "a parse that needs the Resource it is read as, a second input a `TryFrom` has no place for (the media type too)",
    ),
    ty(
        "crates/tutti-midi-ci/",
        "PeState::from_wire",
        CTOR,
        "a parse that needs the media type the bytes came with, a second input a `TryFrom` has no place for",
    ),
    ty(
        "crates/tutti-midi-ci/",
        "ResourceSet::from_selection",
        CTOR,
        "a Set is read from a Link and the entry the user chose (two inputs)",
    ),
    ty(
        "crates/tutti-midi-ci/",
        "ResourceSet::full",
        CTOR,
        "validity depends on the data's Resource and the target's together (two inputs)",
    ),
    ty(
        "crates/tutti-midi-ci/",
        "ResourceTarget::entry",
        CTOR,
        "validity depends on the Resource and the entry together (two inputs)",
    ),
    ty(
        "crates/tutti-midi-ci/",
        "PeChunk::new",
        CTOR,
        "a header is only in chunk 1 and each field has a 14-bit length: validity of the fields together, which no one field's type can carry",
    ),
    ty(
        "crates/tutti-midi-device/",
        "Dialect::of",
        CTOR,
        "a lookup: the dialect of one block of an Endpoint, read with the Profile table the peer reported, refused when the block is not one the device sends to",
    ),
    ty(
        "crates/tutti-sampler/",
        "DiskStreamer::start",
        CTOR,
        "spawning the streamer future: the failure is the executor's (the OS refusing a thread), not a value's",
    ),
    ty(
        "crates/tutti-sampler/",
        "DiskStreamer::manual",
        CTOR,
        "the hand-stepped streamer: the OS can refuse its bell's tick thread",
    ),
    ty(
        "crates/tutti-plugin-types/",
        "WindowHandle::from_window",
        CTOR,
        "reading the platform window handle: the failure is the OS's, not a value's",
    ),
    ty("crates/tutti-nodes/", "FmConfig", WIDTH, FIXED_SOURCE),
    ty(
        "crates/tutti-nodes/src/spatial/ambisonic.rs",
        "AmbisonicEncoderConfig::new",
        WIDTH,
        "the order states the width: an ACN bus of order `n` has `(n + 1)^2` channels, so a width argument could only disagree with it",
    ),
    ty(
        "crates/tutti-nodes/src/spatial/ambisonic.rs",
        "AmbisonicRotatorConfig::new",
        WIDTH,
        "the order states the width: an ACN bus of order `n` has `(n + 1)^2` channels, so a width argument could only disagree with it",
    ),
    ty("crates/tutti-nodes/", "AdditiveConfig", WIDTH, FIXED_SOURCE),
    ty(
        "crates/tutti-nodes/",
        "ClickNodeConfig",
        WIDTH,
        FIXED_SOURCE,
    ),
    ty(
        "crates/tutti-nodes/",
        "AutomationLaneConfig",
        WIDTH,
        FIXED_SOURCE,
    ),
    ty(
        "crates/tutti-nodes/",
        "NoteEnvelopeConfig",
        WIDTH,
        NO_AUDIO_PORT,
    ),
    ty(
        "crates/tutti-nodes/",
        "UnitLaneConfig",
        WIDTH,
        NO_AUDIO_PORT,
    ),
    ty("crates/tutti-nodes/", "KernelConfig", WIDTH, KERNEL_WIDTH),
    ty("crates/tutti-nodes/", "EqBandConfig", WIDTH, MONO_ONLY),
    ty("crates/tutti-nodes/", "MidSideConfig", WIDTH, STEREO_ONLY),
    ty(
        "crates/tutti-nodes/",
        "StereoWidthConfig",
        WIDTH,
        STEREO_ONLY,
    ),
    ty("crates/tutti-nodes/", "AutoPanConfig", WIDTH, STEREO_ONLY),
    ty(
        "crates/tutti-nodes/",
        "PlateReverbConfig",
        WIDTH,
        STEREO_PLATE,
    ),
    ty(
        "crates/tutti-nodes/",
        "FreeverbConfig",
        WIDTH,
        MONO_IN_STEREO_OUT,
    ),
    ty(
        "crates/tutti-nodes/",
        "FdnConfig",
        WIDTH,
        MONO_IN_STEREO_OUT,
    ),
    ty(
        "crates/tutti-nodes/",
        "ShimmerConfig",
        WIDTH,
        MONO_IN_STEREO_OUT,
    ),
    ty("crates/tutti-nodes/", "DelayConfig", WIDTH, OVER_A_LAYOUT),
    ty(
        "crates/tutti-nodes/",
        "ModDelayConfig",
        WIDTH,
        OVER_A_LAYOUT,
    ),
    ty(
        "crates/tutti-dsp/",
        "LimiterBank::true_peak",
        CTOR,
        "a true-peak lookahead is valid only together with the rate: the up-sampler's delay is a fixed count of frames, and whether the lookahead in seconds covers it depends on the rate, so no type can hold the validity before the rate is bound at this call",
    ),
    ty(
        "crates/tutti-dsp/src/delay.rs",
        "DelayLine::read_sample",
        DEFAULT_ENUM,
        "a per-sample read, not a construction: the interpolation is the read's own kernel, chosen by the caller's bank at each call, and the line holds no mode a read could default to",
    ),
    ty("crates/tutti-graph/", "Direct", SETTER, TEST_SUPPORT),
    ty("crates/tutti-session/", "AudioSide", SETTER, TEST_SUPPORT),
    ty(
        "crates/tutti-sampler/",
        "MockTransport",
        SETTER,
        TEST_SUPPORT,
    ),
    ty(
        "crates/tutti-plugin-types/",
        "FakePlugin",
        SETTER,
        TEST_SUPPORT,
    ),
    ty(
        "crates/tutti-plugin-host/",
        "PluginTimeSignature",
        SETTER,
        "a meter slot handle: it installs a project meter into the node's cell, it is not a node, source, spec or config",
    ),
    ty(
        "crates/tutti-rt/",
        "PosWriter",
        SETTER,
        "the one writer of a position ring: a writer handle, not a node, source, spec or config",
    ),
    ty(
        "crates/tutti-nodes/src/click.rs",
        "ClickSettings",
        SETTER,
        "a live control cell the audio thread reads: it is written while the node renders, not a config",
    ),
    ty(
        "crates/tutti-midi-ci/src/initiator.rs",
        "ProfileInitiator::set_on",
        SETTER,
        "the spec's Set Profile On message (M2-102): a request to the peer, not a setter of the initiator",
    ),
    ty(
        "crates/tutti-midi-ci/src/initiator.rs",
        "ProfileInitiator::set_off",
        SETTER,
        "the spec's Set Profile Off message (M2-102): a request to the peer, not a setter of the initiator",
    ),
    ty(
        "crates/tutti-midi-ci/src/pe/limits.rs",
        "",
        RANGE,
        "text_type! writes a text newtype over `Text` or `String` keyed by an alphabet and a byte bound, not a number range; `new(text)` returns the value or `None`",
    ),
    ty(
        "crates/tutti-midi-file/src/file.rs",
        "Track::push",
        &[Rule::ChainResult],
        "an event's tick is valid only against the last event already held: validity of the argument with the state, which no type of the tick can carry",
    ),
    ty(
        "crates/tutti-midi-file/src/file.rs",
        "StandardMidiFile::new",
        CTOR,
        "the tracks, format and division are valid together (one track in format 0, tempo events that make a map): validity of several inputs, which no one type can carry",
    ),
    ty(
        "crates/tutti-midi-file/src/clip.rs",
        "ClipHeader::push",
        &[Rule::ChainResult],
        "a message's tick is valid only against the last message already held: validity of the argument with the state, which no type of the tick can carry",
    ),
    ty(
        "crates/tutti-midi-file/src/clip.rs",
        "ClipSequence::push",
        &[Rule::ChainResult],
        "an event's tick is valid only against the last event already held: validity of the argument with the state, which no type of the tick can carry",
    ),
    ty(
        "crates/tutti-midi-file/src/clip.rs",
        "ClipFile::from_bytes",
        CTOR,
        "decoding a Clip File from its bytes: the failure is the bytes', not a value's",
    ),
    ty(
        "crates/tutti-midi-file/src/file.rs",
        "StandardMidiFile::from_bytes",
        CTOR,
        "decoding a Standard MIDI File from its bytes: the failure is the bytes', not a value's",
    ),
    ty(
        "crates/tutti-midi-file/src/midi_file.rs",
        "MidiFile::from_bytes",
        CTOR,
        "decoding a MIDI file of either kind from its bytes: the failure is the bytes', not a value's",
    ),
    ty(
        "crates/tutti-midi-file/src/clip.rs",
        "ClipFile::new",
        CTOR,
        "a clip is valid only for a header and a sequence whose messages hold the Clip File's rules together: validity of the contents",
    ),
    ty(
        "crates/tutti-midi-file/src/midi_file.rs",
        "MidiFile::read",
        CTOR,
        "reading a MIDI file from a source: the failure is the source's or its bytes', not a value's",
    ),
    ty(
        "crates/tutti-midi-hardware/src/devices.rs",
        "OpenOutput::set_config",
        SETTER,
        "a live command on an open output: the device's UMP Stream negotiation settled a new Protocol and the output follows it in place",
    ),
    ty(
        "crates/tutti-midi-hardware/src/devices.rs",
        "OpenOutput::set_mpe11",
        SETTER,
        "a live command on an open output: the app's MPE 1.1 zones change while events flow, and the Configuration Messages that say so are queued now",
    ),
    ty(
        "crates/tutti-midi-hardware/src/port/manager.rs",
        "HardwareMidiInputs::set_input_timing",
        SETTER,
        "a live command on a running port: the device's timing source changes while events flow",
    ),
    ty(
        "crates/tutti-midi-hardware/src/port/manager.rs",
        "HardwareMidiInputs::set_port_active",
        SETTER,
        "a live command on a running port: it pauses or resumes delivery while the driver runs",
    ),
    ty(
        "crates/tutti-midi-hardware/src/session.rs",
        "MidiSession::set_profile",
        SETTER,
        "the spec's Set Profile On and Off messages (M2-102): a request to the peer through the open session, not a setter of the session",
    ),
    ty(
        "crates/tutti-midi-network/src/command.rs",
        "Command::parse",
        CTOR,
        "decoding a command's header and payload bytes: the failure is the bytes', not a value's",
    ),
    ty(
        "crates/tutti-midi-network/src/command.rs",
        "UmpData::new",
        CTOR,
        "the packets are valid together: their words must total at most 64, a sum no single packet's type can carry",
    ),
    ty(
        "crates/tutti-midi-network/src/discovery.rs",
        "Advertisement::start",
        CTOR,
        "publishing over mDNS: the failure is the OS's or the hostname's, not a value's",
    ),
    ty(
        "crates/tutti-midi-network/src/discovery.rs",
        "FoundHost::from_service",
        CTOR,
        "reading a resolved service record as a Host: the failure is the record's, not a value's",
    ),
    ty(
        "crates/tutti-plugin-types/src/lifecycle.rs",
        "Prepare::with_format",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/lifecycle.rs",
        "Prepare::with_max_block",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/lifecycle.rs",
        "Prepare::with_quantum",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/lifecycle.rs",
        "Prepare::with_mode",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/lifecycle.rs",
        "Prepare::with_rate",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_group",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_measures",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_overview",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_plain_map",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_range",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_role",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_short_name",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_steps",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo::with_unit",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-graph/src/model/bus_edge.rs",
        "NodeBus::with_band",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-graph/src/node/shape.rs",
        "Shape::with_rates",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-graph/src/node/mod.rs",
        "Prepare::with_render_mode",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-graph/src/node/mod.rs",
        "Prepare::with_quantum",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-graph/src/node/time.rs",
        "TransportPosition::with_recording",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-graph/src/node/param_event.rs",
        "ParamRamp::with_shape",
        &[Rule::WithPrefix],
        COPY_WITH_ONE_FIELD,
    ),
    ty(
        "crates/tutti-midi-protocol/src/universal/machine_control/segmentation.rs",
        "MmcSegments::new",
        CTOR,
        "a string is valid as a whole (at most the segmentable length, every byte seven-bit): a check over a byte string, which no one value's type can carry",
    ),
    ty(
        "crates/tutti-midi-protocol/src/universal/machine_control/values.rs",
        "MmcTimeLabel::with_color_frame",
        &[Rule::WithPrefix],
        "a copy of a wire value with one flag replaced (std's `with_` as in `Path::with_extension`), whose getter holds the bare name",
    ),
    ty(
        "crates/tutti-midi-protocol/src/universal/machine_control/values.rs",
        "MmcTimeLabel::with_sign",
        &[Rule::WithPrefix],
        "a copy of a wire value with one flag replaced (std's `with_` as in `Path::with_extension`), whose getter holds the bare name",
    ),
    ty(
        "crates/tutti-midi-runtime/src/clock_map.rs",
        "ClockMap::set_rate",
        SETTER,
        "a live command on a running map: a re-prepare at another rate moves the anchor in place",
    ),
    ty(
        "crates/tutti-codec/src/chain.rs",
        "Chain::set_gain",
        SETTER,
        "a command on a running chain: a normalize's gain is measured from audio that is only written once the chain is open",
    ),
    ty(
        "crates/tutti-midi-runtime/src/live/input_node.rs",
        "MidiInputControls::set_mpe_profile",
        SETTER,
        "a live command through the host's handle on a running node: it publishes a Profile's zones the node adopts at the next block",
    ),
    ty(
        "crates/tutti-midi-runtime/src/live/input_inlets.rs",
        "InputInlets::set_clock",
        SETTER,
        "a live command on shared running inlets: the owner's engine clock arrives after the inlets exist",
    ),
    ty(
        "crates/tutti-midi-runtime/src/live/input_inlets.rs",
        "InputInlets::set_lead",
        SETTER,
        "a live command on shared running inlets: the lead changes while events flow",
    ),
    ty(
        "crates/tutti-ump/src/message.rs",
        "StreamConfig",
        FIELDS,
        "the payload of Stream Configuration Request and Notification: its fields are the UMP wire message's own (protocol, rx_jr, tx_jr), a wire boundary",
    ),
    ty(
        "crates/tutti-plugin-ipc/src/config/bridge.rs",
        "IpcConfig",
        FIELDS,
        "the bridge's configuration as it crosses the host-to-server process boundary (serialized onto the wire): an IPC shape, built from the app's `AudioConfig`",
    ),
    ty(
        "crates/tutti-au-host/src/macos/bus.rs",
        "AuChannelConfig",
        FIELDS,
        "the decoded pair of an AudioUnit `AUChannelInfo` (input and output counts): a mirror of the C ABI record the unit reports, not an app's configuration",
    ),
    ty(
        "crates/tutti-cpal/src/supported.rs",
        "SupportedConfig",
        FIELDS,
        "a reading the device reports (rate range, sample formats, channels), not a configuration an app sets",
    ),
    ty(
        "crates/tutti-midi-runtime/src/live/mpe_translator.rs",
        "MpeTranslator::set_mode",
        SETTER,
        "a live command on a running translator: a mode change stops and resets the channels that leave MPE and sends the events that say so",
    ),
    ty(
        "crates/tutti-midi-runtime/src/live/mpe_translator.rs",
        "MpeTranslator::set_profile",
        SETTER,
        "a live command on a running translator: a Profile change stops and resets the channels that leave MPE and sends the events that say so",
    ),
    ty(
        "crates/tutti-voicing/src/mpe_sender.rs",
        "MpeSender::set_mpe11",
        SETTER,
        "a live command on a running sender: the app's MPE 1.1 zones change, the Configuration Messages that say so are sent at once, and the notes of a zone that changed are forgotten because the receiver ended them",
    ),
    ty(
        "crates/tutti-voicing/src/mpe_sender.rs",
        "MpeSender::set_profile",
        SETTER,
        "a live command on a running sender: the device enabled or disabled a Profile, and the notes of a zone that changed are forgotten because the receiver stopped them",
    ),
    ty(
        "crates/tutti-session/src/graph/observe.rs",
        "Meters::open",
        DEFAULT_ENUM,
        "both alignments are everyday uses (a track's level reads `Raw`, a device's output reads `ToOutput`), \
         so the alignment is the call's choice, not an option",
    ),
];

pub(super) fn allowed(path: &str, s: &Site, a: &Allow) -> bool {
    let under = if a.path == FFI {
        crate_of(path).ends_with("-ffi")
    } else {
        path.starts_with(a.path)
    };
    under
        && match a.owner.split_once("::") {
            Some((owner, func)) => owner == s.owner && s.key.rsplit("::").next() == Some(func),
            None => a.owner.is_empty() || a.owner == s.owner,
        }
        && (a.rules.is_empty() || a.rules.contains(&s.rule))
}
