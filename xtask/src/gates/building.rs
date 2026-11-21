//! `check-building`: a configured thing is built one way, and what is not yet is a
//! row of [`LIST`].
//!
//! A node, a source, a spec, a request or an option is written as
//! `Type::new(required..)` and then chained options (`self`, a bare name, `-> Self`);
//! a running thing changes through its knob. The gate holds that over every shipped
//! crate and counts what is not yet so: a violation without a row of [`LIST`] fails,
//! and so does a row without a violation, so the list only shrinks.
//!
//! **Where it reads.** Every `crates/*/src/**/*.rs` (the `*-ffi` crates too, which
//! [`ALLOW`] excludes), comments and literals blanked, except `tests` and `benches`
//! and `examples` directories, `tests.rs` files and `#[cfg(test)]` items. `testing/`,
//! `xtask/` and `vendor/` are not read.
//!
//! **What it sees.** A `pub fn` (a `pub(..)` item is not public) in an inherent `impl`
//! (a trait impl's methods carry no `pub`), outside the body of a `macro_rules!`. Its
//! *receiver* is by value (`self`, `mut self`), by reference (`&self`, `&mut self`,
//! `&'a self`) or absent. Its *owner* is the last path segment of the `impl`'s type.
//! Its *built type* is the return type with one `Result<..>` or `Option<..>` wrapper (any path to it, as `io::Result<..>`)
//! peeled, generics dropped, when that is `Self` or the owner's name; a return type
//! that is anything else (`Param<U>`, a tuple, a trait object) has none.
//!
//! **The rules** (a row's rule names one):
//!
//! - `with-prefix`: a receiver by value and a name that starts with `with_`.
//! - `chain-result`: a receiver by value and a return type `Result<Self, ..>` or
//!   `Result<Owner, ..>`. A terminal `build(self) -> Result<Product>` is not one: its
//!   built type is not its owner.
//! - `setter`: a receiver by reference and a name that starts with `set_`, on any
//!   owner: wider than the design's node, source or spec, and the difference is carried
//!   by named rows of [`ALLOW`].
//! - `node-ctor`: no receiver, a built type, on a `*Node` owner (a `Result` or
//!   `Option` wrapper counts).
//! - `config-width`: no receiver, return type `Self` or the owner (no wrapper: an
//!   `Option` or `Result` return is not rule 2's "returning `Self`", and `ctor-result`
//!   counts the `Result`), on a *node config*, whose first parameter is not a `ChannelLayout` or `BusTopology`
//!   (the last path segment of a type, or of an `impl Into<_>` or a `T: Into<_>` bound
//!   named in the generics or the `where` clause). A function with no parameter has no
//!   first one. A node config is a `*Config` owner that the shipped tree converts into
//!   a node: an `impl From<Owner..> for X` or `impl TryFrom<Owner..> for X` (generics on
//!   either side, any path prefix) whose `X`'s last path segment ends in `Node` (a type
//!   alias such as `pub type CrossoverNode<F> = KernelNode<..>` is named so), or a
//!   `*Node` owner's no-receiver `pub fn` with a parameter whose type is the owner
//!   (by value, by reference or behind `impl Into<_>`: a constructor like
//!   `new(config: XConfig)`). The width is the first choice of a node; an analysis,
//!   device or transport config is no node config and is not read, whatever its name.
//! - `ctor-result`: no receiver, a built type wrapped in `Result`, on any owner.
//! - `range-macro`: a `macro_rules!` outside `tutti-units` and `tutti-rt`'s `id.rs` (the homes of
//!   `ranged!` and `id!`) whose body holds a
//!   `fn new` of one parameter returning `Option<..>`, the `new(raw) -> Option`
//!   signature of a range-checked type.
//! - `option-arg`: a receiver by value, a built type of `Self` or the owner (no wrapper:
//!   an option returns `Self`), and a parameter after the receiver whose type is an
//!   `Option<..>` (any path to it), an `impl Into<Option<..>>` or a generic bounded
//!   `Into<Option<..>>`: "none" is said by not
//!   calling the option. A terminal `build(self)` whose built type is not its owner is
//!   not one.
//! - `default-enum-arg`: a parameter (after the receiver, any when there is none) whose
//!   type, with `&`, `&mut`, `impl Into<_>` and a `T: Into<_>` bound peeled, names an
//!   enum that has a usual value, in the crate the name resolves to: a `pub enum` with
//!   `#[derive(.. Default ..)]` and a `#[default]` variant, or a `pub enum` with a
//!   hand `impl Default for` (any generics, any path prefix) in its crate. A name
//!   that resolves to a struct, to an enum with no usual value, or to a crate
//!   outside `crates/` is no hit. The usual
//!   value is the option's default, so it is said by not calling the option. Not
//!   one: a receiver by value whose built type (`Self`, the owner, any wrapper) is
//!   its owner, which is how a chained option takes its value, a function whose owner
//!   is the enum itself, a function with no other parameter beside the receiver
//!   (`set_mode(&mut self, mode)`), and a `set_*` function's last parameter (a keyed
//!   setter, `set_mode(&mut self, key, mode)`): those calls exist to pass that value.
//!
//! **Blind spots.** The scan is lexical: it does not see an item a macro writes (the
//! `with-prefix` of a macro-made method), an owner reached through a type alias, a
//! re-export, whether a `pub` type is reachable from the crate root, or what a type
//! is beyond its name's suffix (`node-ctor` reads the suffix, and `config-width` reads
//! the suffix of the node a config converts into or is passed to, matched by name
//! across the tree). `default-enum-arg` resolves a parameter's type name to its crate
//! from the file's `use` items (any form, `as` renames by local name), a qualified
//! path's first segment, or else the file's own crate; a glob import (`use x::*`) or
//! a re-export chain resolves to the importing path's crate only, and a type alias is
//! not seen.
//!
//! **Keys.** `<file>::<Owner>::<fn>`, or `<file>::<macro>` for `range-macro`. No line
//! numbers; items that share a key and a rule (an `impl Foo<f32>` and an
//! `impl Foo<f64>`) share one row.
//!
//! **Lists.** [`LIST`] holds `<crate>  <rule>  <key>` lines (`#` lines and blank lines
//! aside), today's violations by crate; its crate must be the key's. [`ALLOW`] holds
//! the types of another kind, one row per type or per `Type::fn` (a whole-module row only for an FFI
//! crate or a `testing.rs`), each with its reason; a violation a row covers is not
//! counted, and a row that covers no violation fails like a stale baseline row.

use super::buffers::{blocks, ident_at, item_end, split_top, squash, strip_attrs};
use super::lex::{blank, find_word_from, matching, test_ranges};
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// The baseline: today's violations, `<crate>  <rule>  <key>` per line.
pub(crate) const LIST: &str = "xtask/building-baseline.txt";

/// The fewest violations a real run finds; fewer means the walk is broken.
pub(super) const MIN_SITES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Rule {
    WithPrefix,
    ChainResult,
    Setter,
    NodeCtor,
    ConfigWidth,
    CtorResult,
    RangeMacro,
    OptionArg,
    DefaultEnumArg,
}

const RULES: [(Rule, &str); 9] = [
    (Rule::WithPrefix, "with-prefix"),
    (Rule::ChainResult, "chain-result"),
    (Rule::Setter, "setter"),
    (Rule::NodeCtor, "node-ctor"),
    (Rule::ConfigWidth, "config-width"),
    (Rule::CtorResult, "ctor-result"),
    (Rule::RangeMacro, "range-macro"),
    (Rule::OptionArg, "option-arg"),
    (Rule::DefaultEnumArg, "default-enum-arg"),
];

impl Rule {
    fn name(self) -> &'static str {
        // `RULES` lists the variants in declaration order.
        RULES[self as usize].1
    }

    fn parse(name: &str) -> Option<Rule> {
        RULES.iter().find(|(_, n)| *n == name).map(|r| r.0)
    }
}

/// A type of another kind: the violations of `rules` (all when empty) of items
/// under `path`, owned by `owner` (any when empty; `Type::fn` names one function), are
/// not counted. A row that covers
/// no violation fails the gate.
pub(super) struct Allow {
    /// A repo path prefix: a crate's directory, or a file.
    pub(super) path: &'static str,
    pub(super) owner: &'static str,
    pub(super) rules: &'static [Rule],
    pub(super) reason: &'static str,
}

/// The path of every `*-ffi` crate.
const FFI: &str = "crates/*-ffi/";

const GRAPH: &str = "crates/tutti-graph/";

const NODE_AUTHOR: &str = "tutti-graph's node-author API: what a node declares and what a \
    node is prepared with, not what an app configures";

const CTOR: &[Rule] = &[Rule::CtorResult];
const RANGE: &[Rule] = &[Rule::RangeMacro];
const SETTER: &[Rule] = &[Rule::Setter];
const WIDTH: &[Rule] = &[Rule::ConfigWidth];
const DEFAULT_ENUM: &[Rule] = &[Rule::DefaultEnumArg];

/// An allow row for one type `owner` under `path`, for `rules`.
const fn ty(
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
pub(super) const ALLOW: &[Allow] = &[
    Allow {
        path: GRAPH,
        owner: "Shape",
        rules: &[],
        reason: NODE_AUTHOR,
    },
    Allow {
        path: GRAPH,
        owner: "Prepare",
        rules: &[],
        reason: NODE_AUTHOR,
    },
    Allow {
        path: GRAPH,
        owner: "Forked",
        rules: &[],
        reason: NODE_AUTHOR,
    },
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
        "crates/tutti-plugin-types/src/channels.rs",
        "ChannelTable::of_planes",
        CTOR,
        "a table has a fixed number of slots: the width of a block is a runtime fact the slots must hold",
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
        "UdpDriver::bind",
        CTOR,
        "binding a UDP socket: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-plugin-ipc/src/transport/sys/unix.rs",
        "Watch::of",
        CTOR,
        "duplicating a descriptor: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-plugin-ipc/src/transport/sys/windows.rs",
        "Watch::of",
        CTOR,
        "duplicating a handle: the failure is the OS's, not a value's",
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
        "crates/tutti-codec/",
        "FileEncoder::create",
        CTOR,
        "creating a staged file to encode into: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-codec/",
        "FileEncoder::create_planned",
        CTOR,
        "creating a staged file to encode into: the failure is the OS's, not a value's",
    ),
    ty(
        "crates/tutti-tempo/",
        "TempoMap::from_beats",
        CTOR,
        "a conversion from measured beats that can fail on the input: the beats' count and order are the caller's, not a value's",
    ),
    ty(
        "crates/tutti-platform/",
        "Stamp::of",
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
    ty(
        "crates/tutti-nodes/",
        "OscillatorConfig",
        WIDTH,
        FIXED_SOURCE,
    ),
    ty("crates/tutti-nodes/", "LfoConfig", WIDTH, FIXED_SOURCE),
    ty("crates/tutti-nodes/", "FmConfig", WIDTH, FIXED_SOURCE),
    ty("crates/tutti-nodes/", "AdditiveConfig", WIDTH, FIXED_SOURCE),
    ty("crates/tutti-nodes/", "NoiseConfig", WIDTH, FIXED_SOURCE),
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
    ty(
        "crates/tutti-nodes/",
        "ForeignLaneConfig",
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
        "HrtfBinauralConfig",
        WIDTH,
        "a stereo-only node: the binaural renderer folds a pair to mono and writes the left and right ear, so the width is not a choice and the config takes none",
    ),
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
        "LimiterBank::detecting",
        CTOR,
        "a true-peak lookahead is valid only together with the rate: the up-sampler's delay is a fixed count of frames, and whether the lookahead in seconds covers it depends on the rate, so no type can hold the validity before the rate is bound at this call",
    ),
    ty("crates/tutti-graph/", "Direct", SETTER, TEST_SUPPORT),
    ty("crates/tutti-graph/", "Reference", SETTER, TEST_SUPPORT),
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
        "MidiFile::smf",
        CTOR,
        "the tracks, format and division are valid together (one track in format 0, tempo events that make a map): validity of several inputs, which no one type can carry",
    ),
    ty(
        "crates/tutti-midi-file/src/file.rs",
        "MidiFile::clip",
        CTOR,
        "a clip is valid only for a track whose events and header hold the Clip File's rules together: validity of the track's contents",
    ),
    ty(
        "crates/tutti-midi-file/src/file.rs",
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
        "crates/tutti-session/src/graph/observe.rs",
        "Meters::open",
        DEFAULT_ENUM,
        "both alignments are everyday uses (a track's level reads `Raw`, a device's output reads `ToOutput`), \
         so the alignment is the call's choice, not an option",
    ),
];

/// A violation: its key and rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Site {
    pub(super) key: String,
    pub(super) owner: String,
    pub(super) rule: Rule,
}

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "tests" | "benches" | "examples")
}

fn source_files(tree: &dyn Tree) -> Vec<String> {
    tree.files("crates", &skipped)
        .into_iter()
        .filter(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            p.ends_with(".rs")
                && parts.get(2) == Some(&"src")
                && parts.last() != Some(&"tests.rs")
                && !parts.contains(&"tests")
        })
        .collect()
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Recv {
    Value,
    Ref,
    None,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Wrap {
    Plain,
    Result,
    Option,
}

fn receiver(first: Option<&str>) -> Recv {
    let Some(first) = first else {
        return Recv::None;
    };
    let t = squash(strip_attrs(first));
    if t.starts_with('&') {
        // `&self`, `&mut self`, `&'a self`, `&'a mut self`.
        let rest = t.trim_start_matches('&');
        let rest = match rest.strip_prefix('\'') {
            Some(l) => l.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_'),
            None => rest,
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim();
        return if rest == "self" {
            Recv::Ref
        } else {
            Recv::None
        };
    }
    let rest = t.strip_prefix("mut ").unwrap_or(&t);
    if rest == "self" {
        return Recv::Value;
    }
    match rest.strip_prefix("self:") {
        Some(ty) if ty.trim_start().starts_with('&') => Recv::Ref,
        Some(_) => Recv::Value,
        None => Recv::None,
    }
}

/// The last path segment of `ty` without its generics.
fn bare(ty: &str) -> &str {
    let ty = ty.trim();
    let end = ty.find('<').unwrap_or(ty.len());
    ty[..end].rsplit("::").next().unwrap_or("").trim()
}

/// The wrapper of a return type and whether what it wraps is `Self` or `owner`.
fn built(ret: &str, owner: &str) -> Option<Wrap> {
    let r = squash(ret);
    // A wrapper is named by its last path segment: `io::Result<Self>` is a `Result`.
    let wrapper = r.find('<').and_then(|lt| {
        let seg = r[..lt].rsplit("::").next().unwrap_or("");
        match seg {
            "Result" => Some((Wrap::Result, &r[lt + 1..])),
            "Option" => Some((Wrap::Option, &r[lt + 1..])),
            _ => None,
        }
    });
    let (wrap, inner) = wrapper.unwrap_or((Wrap::Plain, r.as_str()));
    let ty = bare(split_top(inner)[0]).trim_end_matches('>');
    (ty == "Self" || ty == owner).then_some(wrap)
}

/// Whether a parameter `pat: ty` names a width: a `ChannelLayout` or `BusTopology`,
/// directly, behind `impl Into<_>`, or as a generic with that `Into` bound.
fn is_width(param: &str, bounds: &str) -> bool {
    let p = squash(strip_attrs(param));
    let Some((_, ty)) = p.split_once(':') else {
        return false;
    };
    let ty = ty.trim();
    let wide = |t: &str| matches!(bare(t), "ChannelLayout" | "BusTopology");
    if wide(ty) {
        return true;
    }
    let into = |t: &str| {
        t.strip_prefix("Into<")
            .and_then(|r| r.strip_suffix('>'))
            .is_some_and(wide)
    };
    if let Some(t) = ty.strip_prefix("impl ") {
        return into(t.trim());
    }
    // A generic `L` bounded `L: Into<ChannelLayout>` in the generics or `where`.
    let b = squash(bounds);
    for w in ["ChannelLayout", "BusTopology"] {
        let pat = format!("{ty}:Into<{w}>");
        if b.contains(&pat) || b.contains(&format!("{ty}: Into<{w}>")) {
            return true;
        }
    }
    false
}

/// Whether a parameter `pat: ty` is an `Option<..>` (any path to it), an
/// `impl Into<Option<..>>`, or a generic bounded `Into<Option<..>>` in `bounds`
/// (the generics and the `where` clause).
fn takes_option(param: &str, bounds: &str) -> bool {
    let p = squash(strip_attrs(param));
    let Some((_, ty)) = p.split_once(':') else {
        return false;
    };
    let ty = ty.trim();
    if let Some(t) = ty.strip_prefix("impl ") {
        return t
            .trim()
            .strip_prefix("Into<")
            .is_some_and(|r| bare(r) == "Option");
    }
    if bare(ty) == "Option" && ty.contains('<') {
        return true;
    }
    let b = squash(bounds);
    b.contains(&format!("{ty}:Into<Option<")) || b.contains(&format!("{ty}: Into<Option<"))
}

/// The type path a parameter `pat: ty` takes, without generics, with `&`, `&mut`, a
/// lifetime, `impl Into<_>` and a generic's `Into<_>` bound peeled.
fn param_path(param: &str, bounds: &str) -> Option<String> {
    let p = squash(strip_attrs(param));
    let (_, ty) = p.split_once(':')?;
    let peel = |t: &str| -> String {
        let mut t = t.trim().trim_start_matches('&').trim_start();
        if let Some(r) = t.strip_prefix('\'') {
            t = r
                .trim_start_matches(|c: char| c.is_alphanumeric() || c == '_')
                .trim_start();
        }
        t.strip_prefix("mut ").unwrap_or(t).trim().to_owned()
    };
    let inner = |t: &str| -> Option<String> {
        let r = t.strip_prefix("Into<")?.strip_suffix('>')?;
        Some(peel(r))
    };
    let ty = peel(ty);
    let ty = if let Some(t) = ty.strip_prefix("impl ") {
        inner(t.trim())?
    } else {
        let b = squash(bounds);
        let at = b
            .find(&format!("{ty}:Into<"))
            .or_else(|| b.find(&format!("{ty}: Into<")));
        match at {
            Some(i) => {
                let rest = &b[i + ty.len()..];
                let rest = rest.trim_start_matches(':').trim_start();
                let close = matching(rest, 4, b'<', b'>')?;
                inner(&rest[..=close]).unwrap_or(ty)
            }
            None => ty,
        }
    };
    let path = ty[..ty.find('<').unwrap_or(ty.len())]
        .trim()
        .trim_start_matches("::");
    (!path.is_empty()
        && path
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':'))
    .then(|| path.to_owned())
}

/// The crate directory name a path's first segment names: `tutti_x` is `tutti-x`,
/// `crate`, `self`, `super` and a local module are `own`; `None` for `std`, `core`
/// and `alloc`.
fn first_crate(seg: &str, own: &str) -> Option<String> {
    match seg {
        "std" | "core" | "alloc" => None,
        s if s.starts_with("tutti_") => Some(s.replace('_', "-")),
        _ => Some(own.to_owned()),
    }
}

/// What a file's `use` items bring in: each local name with the crate it comes from,
/// and the crates of its glob imports.
#[derive(Default)]
struct Imports {
    /// A local name: the crate it comes from and the name it has there.
    names: BTreeMap<String, (Option<String>, String)>,
    globs: BTreeSet<String>,
}

impl Imports {
    fn add(&mut self, tree: &str, prefix: &[String], own: &str) {
        let t = tree.trim();
        if let Some(b) = t.find('{') {
            let Some(close) = matching(t, b, b'{', b'}') else {
                return;
            };
            let mut pre = prefix.to_vec();
            pre.extend(segments(&t[..b]));
            for part in split_top(&t[b + 1..close]) {
                self.add(part, &pre, own);
            }
            return;
        }
        let (path, alias) = match find_word_from(t, "as", 0) {
            Some(a) => (&t[..a], Some(t[a + 2..].trim())),
            None => (t, None),
        };
        let mut full = prefix.to_vec();
        full.extend(segments(path));
        let Some(first) = full.first() else { return };
        let krate = first_crate(first, own);
        match full.last().map(String::as_str) {
            Some("*") => {
                self.globs.extend(krate);
            }
            Some("self") if full.len() >= 2 => {
                let local = full[full.len() - 2].clone();
                let orig = local.clone();
                self.names.insert(local, (krate, orig));
            }
            Some(last) => {
                let local = alias.unwrap_or(last).to_owned();
                self.names.insert(local, (krate, last.to_owned()));
            }
            None => {}
        }
    }

    /// The `(crate, name)` a type path may name, in a file of crate `own`.
    fn resolve(&self, path: &str, own: &str) -> Vec<(String, String)> {
        let segs = segments(path);
        let Some(last) = segs.last() else {
            return Vec::new();
        };
        let pair = |c: &String| (c.clone(), last.clone());
        match segs.as_slice() {
            [name] => match self.names.get(name) {
                Some((c, orig)) => c.iter().map(|c| (c.clone(), orig.clone())).collect(),
                None => std::iter::once(own.to_owned())
                    .chain(self.globs.iter().cloned())
                    .map(|c| (c, last.clone()))
                    .collect(),
            },
            [first, ..] => match self.names.get(first) {
                Some((c, _)) => c.iter().map(pair).collect(),
                None => first_crate(first, own).iter().map(pair).collect(),
            },
            [] => Vec::new(),
        }
    }
}

fn segments(path: &str) -> Vec<String> {
    path.split("::")
        .map(|x| x.trim().to_owned())
        .filter(|x| !x.is_empty())
        .collect()
}

/// The type an `impl Default for X` header (generics, a path prefix and a `where`
/// clause allowed) is for.
fn default_target(header: &str) -> Option<String> {
    let mut h = header.trim();
    if h.starts_with('<') {
        h = h[matching(h, 0, b'<', b'>')? + 1..].trim();
    }
    if let Some(w) = find_word_from(h, "where", 0) {
        h = h[..w].trim();
    }
    let f = find_word_from(h, "for", 0)?;
    (bare(&h[..f]) == "Default").then(|| bare(h[f + 3..].trim()).to_owned())
}

fn macro_bodies(code: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "macro_rules", from) {
        from = at + 11;
        let rest = code[from..].trim_start();
        let Some(rest) = rest.strip_prefix('!') else {
            continue;
        };
        let (name, after) = ident_at(code, code.len() - rest.len());
        let name = name.to_owned();
        let Some(open) = code[after..].find(['{', '(', '[']).map(|i| after + i) else {
            continue;
        };
        let (o, c) = match code.as_bytes()[open] {
            b'{' => (b'{', b'}'),
            b'(' => (b'(', b')'),
            _ => (b'[', b']'),
        };
        if let Some(close) = matching(code, open, o, c) {
            out.push((open, close, name));
            from = close;
        }
    }
    out
}

/// A `fn new` of one parameter returning `Option<..>` inside `body`.
fn writes_range_type(body: &str) -> bool {
    let mut from = 0;
    while let Some(at) = find_word_from(body, "new", from) {
        from = at + 3;
        if !body[..at].trim_end().ends_with("fn") {
            continue;
        }
        let rest = body[from..].trim_start();
        let Some(rest) = rest.strip_prefix('(') else {
            continue;
        };
        let off = body.len() - rest.len() - 1;
        let Some(close) = matching(body, off, b'(', b')') else {
            continue;
        };
        let params = &body[off + 1..close];
        let ret = squash(
            body[close + 1..]
                .trim_start()
                .split(['{', ';'])
                .next()
                .unwrap_or(""),
        );
        let one = split_top(params)
            .iter()
            .filter(|p| !p.trim().is_empty())
            .count()
            == 1;
        if one && ret.starts_with("->Option<") {
            return true;
        }
    }
    false
}

/// The violations of one file's text.
#[cfg(test)]
pub(super) fn scan(path: &str, src: &str) -> Vec<Site> {
    let parsed = parse_file(path, src);
    let nodes = parsed.nodes.clone();
    let usual = Usual::of(std::iter::once(&parsed));
    parsed.finish(&nodes, &usual)
}

/// One file's reading: its violations, its `config-width` candidates (kept apart until
/// the tree's node configs are known) and the `*Config` names it converts into a node.
struct Parsed {
    krate: String,
    sites: BTreeSet<Site>,
    widths: Vec<Site>,
    nodes: BTreeSet<String>,
    /// `default-enum-arg` candidates: a site and the `(crate, name)` each parameter's
    /// type may be.
    args: Vec<(Site, Vec<(String, String)>)>,
    /// The `pub enum`s that derive `Default`, and every `pub enum`.
    derived: BTreeSet<String>,
    pub_enums: BTreeSet<String>,
    /// The types an `impl Default for` names.
    hand: BTreeSet<String>,
}

/// The `(crate, name)` of every `pub enum` that has a usual value: a derived `Default`
/// with a `#[default]` variant, or a hand `impl Default for` in its crate.
#[derive(Default)]
struct Usual {
    enums: BTreeSet<(String, String)>,
}

impl Usual {
    fn of<'a>(files: impl Iterator<Item = &'a Parsed> + Clone) -> Usual {
        let mut u = Usual::default();
        let mut impls = BTreeSet::new();
        let mut enums = BTreeSet::new();
        for p in files {
            u.enums
                .extend(p.derived.iter().map(|n| (p.krate.clone(), n.clone())));
            impls.extend(p.hand.iter().map(|n| (p.krate.clone(), n.clone())));
            enums.extend(p.pub_enums.iter().map(|n| (p.krate.clone(), n.clone())));
        }
        u.enums.extend(impls.intersection(&enums).cloned());
        u
    }
}

impl Parsed {
    /// The violations, with a `config-width` candidate kept when its owner is in `nodes`
    /// and a `default-enum-arg` candidate kept when a parameter's type is in `usual`.
    fn finish(self, nodes: &BTreeSet<String>, usual: &Usual) -> Vec<Site> {
        let mut sites = self.sites;
        sites.extend(self.widths.into_iter().filter(|s| nodes.contains(&s.owner)));
        sites.extend(self.args.into_iter().filter_map(|(s, names)| {
            names
                .iter()
                .any(|c| c.1 != s.owner && usual.enums.contains(c))
                .then_some(s)
        }));
        sites.into_iter().collect()
    }
}

/// The `*Config` an `impl From<Config..> for ..Node..` or `impl TryFrom<Config..> for
/// ..Node..` header converts into a node, if it is one.
fn converted_config(header: &str) -> Option<String> {
    let mut h = header.trim();
    if h.starts_with('<') {
        h = h[matching(h, 0, b'<', b'>')? + 1..].trim();
    }
    if let Some(w) = find_word_from(h, "where", 0) {
        h = h[..w].trim();
    }
    let f = find_word_from(h, "for", 0)?;
    let (tr, target) = (h[..f].trim(), h[f + 3..].trim());
    let lt = tr.find('<')?;
    if !matches!(bare(&tr[..lt]), "From" | "TryFrom") {
        return None;
    }
    let arg = split_top(tr[lt + 1..].trim_end().strip_suffix('>')?)[0];
    let config = bare(arg.trim().trim_start_matches('&'));
    let node = bare(target);
    (config.ends_with("Config") && config.len() > 6 && node.ends_with("Node") && node.len() > 4)
        .then(|| config.to_owned())
}

/// The `*Config` a parameter `pat: ty` takes, directly or behind `impl Into<_>`.
fn config_param(param: &str) -> Option<String> {
    let p = squash(strip_attrs(param));
    let (_, ty) = p.split_once(':')?;
    let ty = ty.trim().trim_start_matches('&');
    let ty = ty
        .strip_prefix("implInto<")
        .or_else(|| ty.strip_prefix("impl Into<"))
        .map_or(ty, |t| t.trim_end_matches('>'));
    let name = bare(ty);
    (name.ends_with("Config") && name.len() > 6).then(|| name.to_owned())
}

fn parse_file(path: &str, src: &str) -> Parsed {
    let code = blank(src);
    let tests = test_ranges(&code);
    let in_test = |p: usize| tests.iter().any(|&(a, z)| a <= p && p <= z);
    let macros = macro_bodies(&code);
    let in_macro = |p: usize| macros.iter().any(|(a, z, _)| *a < p && p < *z);
    let blocks = blocks(&code);
    let owner_of = |p: usize| {
        blocks
            .iter()
            .filter(|b| b.open < p && p < b.close)
            .max_by_key(|b| b.open)
    };
    let in_macro_home =
        path.starts_with("crates/tutti-units/") || path == "crates/tutti-rt/src/id.rs";
    let mut sites = BTreeSet::new();
    let mut widths = Vec::new();
    let mut nodes = BTreeSet::new();
    let own = crate_of(path);
    let mut imports = Imports::default();
    let mut u = 0;
    while let Some(p) = find_word_from(&code, "use", u) {
        u = p + 3;
        if in_test(p) || in_macro(p) {
            continue;
        }
        let end = code[u..].find(';').map_or(code.len(), |i| u + i);
        imports.add(&code[u..end], &[], own);
    }
    let mut args = Vec::new();
    let mut derived = BTreeSet::new();
    let mut pub_enums = BTreeSet::new();
    let mut hand = BTreeSet::new();
    let mut at = 0;
    while let Some(p) = find_word_from(&code, "impl", at) {
        at = p + 4;
        let head = code[..p].trim_end();
        let head_ok = head.is_empty()
            || head.ends_with(['}', ';', '{', ']'])
            || head.ends_with("unsafe")
            || head.ends_with("default");
        if !head_ok || in_test(p) {
            continue;
        }
        let end = code[at..].find(['{', ';']).map_or(code.len(), |i| at + i);
        nodes.extend(converted_config(&code[at..end]));
        hand.extend(default_target(&code[at..end]));
    }
    let mut at = 0;
    while let Some(e) = find_word_from(&code, "enum", at) {
        at = e + 4;
        let before = code[..e].trim_end();
        let Some(pub_at) = before.len().checked_sub(3) else {
            continue;
        };
        if !before.ends_with("pub")
            || in_test(e)
            || before[..pub_at].ends_with(|c: char| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let (name, after) = ident_at(&code, e + 4);
        if name.is_empty() {
            continue;
        }
        pub_enums.insert(name.to_owned());
        let start = code[..pub_at].rfind(['}', ';', '{']).map_or(0, |i| i + 1);
        let attrs = &code[start..pub_at];
        let Some(open) = code[after..].find('{').map(|i| after + i) else {
            continue;
        };
        let Some(close) = matching(&code, open, b'{', b'}') else {
            continue;
        };
        if attrs.contains("derive")
            && find_word_from(attrs, "Default", 0).is_some()
            && squash(&code[open..close]).contains("#[default]")
        {
            derived.insert(name.to_owned());
        }
    }
    if !in_macro_home {
        for (open, close, name) in &macros {
            if writes_range_type(&code[open + 1..*close]) {
                sites.insert(Site {
                    key: format!("{path}::{name}"),
                    owner: String::new(),
                    rule: Rule::RangeMacro,
                });
            }
        }
    }
    let mut from = 0;
    while let Some(p) = find_word_from(&code, "pub", from) {
        from = p + 3;
        if in_test(p) || in_macro(p) || code[from..].trim_start().starts_with('(') {
            continue;
        }
        let mut at = from;
        let (w, end) = loop {
            let (w, end) = ident_at(&code, at);
            match w {
                "const" | "async" | "unsafe" | "default" => at = end,
                "extern" => {
                    let rest = code[end..].trim_start();
                    at = end;
                    if rest.starts_with('"') {
                        let open = code[end..].find('"').map_or(end, |q| end + q);
                        at = code[open + 1..].find('"').map_or(open, |q| open + 1 + q) + 1;
                    }
                }
                _ => break (w, end),
            }
        };
        if w != "fn" {
            continue;
        }
        let Some(block) = owner_of(p) else { continue };
        let (name, mut cur) = ident_at(&code, end);
        let mut bounds = String::new();
        if code[cur..].trim_start().starts_with('<') {
            let lt = cur + (code[cur..].len() - code[cur..].trim_start().len());
            let Some(gt) = matching(&code, lt, b'<', b'>') else {
                continue;
            };
            bounds.push_str(&code[lt..=gt]);
            cur = gt + 1;
        }
        let rest = code[cur..].trim_start();
        if !rest.starts_with('(') {
            continue;
        }
        let open = code.len() - rest.len();
        let Some(close) = matching(&code, open, b'(', b')') else {
            continue;
        };
        let params: Vec<&str> = split_top(&code[open + 1..close])
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let stop = item_end(&code, close + 1);
        let tail = &code[close + 1..stop];
        let (ret, wh) = match find_word_from(tail, "where", 0) {
            Some(w) => (&tail[..w], &tail[w..]),
            None => (tail, ""),
        };
        bounds.push_str(wh);
        let ret = ret.trim().strip_prefix("->").unwrap_or("").trim();
        let owner = block.owner.as_str();
        let recv = receiver(params.first().copied());
        let ty = built(ret, owner);
        let key = format!("{path}::{owner}::{name}");
        let mut hit = |rule| {
            sites.insert(Site {
                key: key.clone(),
                owner: owner.to_owned(),
                rule,
            });
        };
        let chained = recv == Recv::Value && ty.is_some();
        let skip = usize::from(recv != Recv::None);
        if !chained && params.len() > skip + 1 {
            // A keyed setter's last parameter is the value it sets.
            let set = params.len() - usize::from(name.starts_with("set_"));
            let names: Vec<(String, String)> = params[..set]
                .iter()
                .skip(skip)
                .filter_map(|p| param_path(p, &bounds))
                .flat_map(|path| imports.resolve(&path, own))
                .collect();
            if !names.is_empty() {
                args.push((
                    Site {
                        key: key.clone(),
                        owner: owner.to_owned(),
                        rule: Rule::DefaultEnumArg,
                    },
                    names,
                ));
            }
        }
        match recv {
            Recv::Value => {
                if name.starts_with("with_") {
                    hit(Rule::WithPrefix);
                }
                if ty == Some(Wrap::Result) {
                    hit(Rule::ChainResult);
                }
                if ty == Some(Wrap::Plain)
                    && params.iter().skip(1).any(|p| takes_option(p, &bounds))
                {
                    hit(Rule::OptionArg);
                }
            }
            Recv::Ref => {
                if name.starts_with("set_") {
                    hit(Rule::Setter);
                }
            }
            Recv::None => {
                if let Some(wrap) = ty {
                    if owner.ends_with("Node") && owner.len() > 4 {
                        hit(Rule::NodeCtor);
                    }
                    if wrap == Wrap::Result {
                        hit(Rule::CtorResult);
                    }
                }
                if owner.ends_with("Node") && owner.len() > 4 {
                    nodes.extend(params.iter().find_map(|p| config_param(p)));
                }
                if ty == Some(Wrap::Plain)
                    && owner.ends_with("Config")
                    && owner.len() > 6
                    && !params.first().is_some_and(|f| is_width(f, &bounds))
                {
                    widths.push(Site {
                        key: key.clone(),
                        owner: owner.to_owned(),
                        rule: Rule::ConfigWidth,
                    });
                }
            }
        }
        from = close;
    }
    Parsed {
        krate: crate_of(path).to_owned(),
        sites,
        widths,
        nodes,
        args,
        derived,
        pub_enums,
        hand,
    }
}

fn crate_of(path: &str) -> &str {
    path.split('/').nth(1).unwrap_or("")
}

fn allowed(path: &str, s: &Site, a: &Allow) -> bool {
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

/// A baseline row: crate, rule, key.
type Row = (String, Rule, String);

fn parse(text: &str) -> Result<BTreeSet<Row>, Vec<String>> {
    let mut rows = BTreeSet::new();
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = n + 1;
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [krate, rule, key] = parts[..] else {
            errors.push(format!(
                "  {LIST}:{at}: not `<crate>  <rule>  <key>`: {line}"
            ));
            continue;
        };
        let Some(rule) = Rule::parse(rule) else {
            errors.push(format!("  {LIST}:{at}: `{rule}` is not a rule"));
            continue;
        };
        if key.split('/').nth(1) != Some(krate) {
            errors.push(format!("  {LIST}:{at}: `{key}` is not in crate `{krate}`"));
        }
        if !rows.insert((krate.to_owned(), rule, key.to_owned())) {
            errors.push(format!("  {LIST}:{at}: `{line}` is listed twice"));
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

/// The violations of the tree that `allow` does not cover.
fn found(tree: &dyn Tree, allow: &[Allow]) -> (BTreeSet<Row>, BTreeSet<usize>) {
    let mut sites = BTreeSet::new();
    let mut used = BTreeSet::new();
    let mut parsed = Vec::new();
    let mut nodes = BTreeSet::new();
    for path in source_files(tree) {
        let Some(src) = tree.read(&path) else {
            continue;
        };
        let p = parse_file(&path, &src);
        nodes.extend(p.nodes.iter().cloned());
        parsed.push((path, p));
    }
    let usual = Usual::of(parsed.iter().map(|(_, p)| p));
    for (path, p) in parsed {
        for s in p.finish(&nodes, &usual) {
            let covered: Vec<usize> = (0..allow.len())
                .filter(|&i| allowed(&path, &s, &allow[i]))
                .collect();
            if covered.is_empty() {
                sites.insert((crate_of(&path).to_owned(), s.rule, s.key));
            }
            used.extend(covered);
        }
    }
    (sites, used)
}

pub(super) fn check(tree: &dyn Tree, allow: &[Allow], min_sites: usize) -> Report {
    let mut r = Report::default();
    let Some(text) = tree.read(LIST) else {
        r.fail(format!("check-building: FAIL: {LIST} is missing"));
        return r;
    };
    let rows = match parse(&text) {
        Ok(rows) => rows,
        Err(errors) => {
            r.fail(format!("check-building: FAIL ({LIST} is malformed):"));
            errors.into_iter().for_each(|e| r.fail(e));
            return r;
        }
    };
    let (sites, used) = found(tree, allow);
    if sites.len() < min_sites {
        r.fail(format!(
            "check-building: FAIL: {} violations found, at least {min_sites} expected; the walk is broken",
            sites.len()
        ));
        return r;
    }
    let fmt = |(c, rule, k): &Row| format!("{c}  {}  {k}", rule.name());
    let new: Vec<_> = sites.difference(&rows).collect();
    let stale: Vec<_> = rows.difference(&sites).collect();
    if !new.is_empty() {
        r.fail(
            "check-building: FAIL (a configured thing is built another way than the one shape; \
             build it the one way, or an allow row in the gate says why it is another kind):",
        );
        new.into_iter()
            .for_each(|row| r.fail(format!("  {}", fmt(row))));
        r.say(
            "check-building: the builders of another kind (ALLOW in xtask/src/gates/building.rs):",
        );
        for a in allow {
            let owner = if a.owner.is_empty() { "*" } else { a.owner };
            r.say(format!("  {} {owner}: {}", a.path, a.reason));
        }
    }
    if !stale.is_empty() {
        r.fail(format!(
            "check-building: FAIL (a row of {LIST} names no violation; delete it):"
        ));
        stale
            .into_iter()
            .for_each(|row| r.fail(format!("  {}", fmt(row))));
    }
    let idle: Vec<_> = (0..allow.len()).filter(|i| !used.contains(i)).collect();
    if !idle.is_empty() {
        r.fail("check-building: FAIL (a row of ALLOW covers no violation; delete it):");
        for i in idle {
            let a = &allow[i];
            let owner = if a.owner.is_empty() { "*" } else { a.owner };
            r.fail(format!("  {} {owner}: {}", a.path, a.reason));
        }
    }
    if r.passed() {
        r.say("check-building: OK");
    }
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for (c, _, _) in &sites {
        *by.entry(c).or_default() += 1;
    }
    let counts: Vec<String> = by.iter().map(|(c, n)| format!("{c} {n}")).collect();
    r.say(format!(
        "check-building: {} violations, {} rows in {LIST} ({})",
        sites.len(),
        rows.len(),
        counts.join(", ")
    ));
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const F: &str = "crates/a/src/lib.rs";

    fn run(list: &str, src: &str, allow: &[Allow]) -> Report {
        check(&Mem::with(&[(LIST, list), (F, src)]), allow, 0)
    }

    fn rules(src: &str) -> Vec<(String, &'static str)> {
        scan(F, src)
            .into_iter()
            .map(|s| (s.key.rsplit("::").next().unwrap().to_owned(), s.rule.name()))
            .collect()
    }

    fn one(name: &str, rule: &'static str) -> Vec<(String, &'static str)> {
        vec![(name.to_owned(), rule)]
    }

    #[test]
    fn a_chained_option_named_with_is_a_violation_and_a_bare_name_is_not() {
        let bad = "pub struct T; impl T { pub fn with_gain(mut self, g: f32) -> Self { self } }";
        assert_eq!(rules(bad), one("with_gain", "with-prefix"));
        let ok = "pub struct T; impl T { pub fn gain(mut self, g: f32) -> Self { self }\n\
                  pub fn with_view(&self) -> u8 { 0 } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_chained_option_returning_result_self_is_a_violation_and_a_terminal_build_is_not() {
        let bad =
            "pub struct T; impl T { pub fn depth(self, d: u8) -> Result<Self, E> { Ok(self) } }";
        assert_eq!(rules(bad), one("depth", "chain-result"));
        let bare = "pub struct T; impl T { pub fn depth(self) -> Result<T> { Ok(self) } }";
        assert_eq!(rules(bare), one("depth", "chain-result"));
        let ok = "pub struct B; impl B { pub fn build(self) -> Result<Product, E> { todo!() }\n\
                  pub fn depth(self, d: u8) -> Self { self } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_chained_option_taking_an_option_is_a_violation_and_other_shapes_are_not() {
        for bad in [
            "impl T { pub fn pool(mut self, p: Option<Arc<P>>) -> Self { self } }",
            "impl T { pub fn pool(self, p: std::option::Option<u8>) -> T { self } }",
            "impl T { pub fn pool(self, p: core::option::Option<u8>) -> Self { self } }",
            "impl T { pub fn pool(self, p: impl Into<Option<u8>>) -> Self { self } }",
            "impl T { pub fn pool(self, p: ::core::option::Option<u8>) -> Self { self } }",
            "impl T { pub fn pool(self, mut p: Option<u8>) -> Self { self } }",
            "impl T { pub fn pool<O: Into<Option<u8>>>(self, p: O) -> Self { self } }",
            "impl T { pub fn pool<O>(self, p: O) -> Self where O: Into<Option<u8>> { self } }",
        ] {
            assert_eq!(rules(bad), one("pool", "option-arg"), "{bad}");
        }
        for ok in [
            "impl T { pub fn pool(&self, p: Option<u8>) -> Self { todo!() } }",
            "impl T { pub fn pool(self, p: Option<u8>) -> Option<Self> { Some(self) } }",
            "impl T { pub fn build(self, p: Option<u8>) -> Product { todo!() } }",
            "pub fn pool(p: Option<u8>) -> Self { todo!() }",
            "impl T { pub(crate) fn pool(self, p: Option<u8>) -> Self { self } }",
            "impl T { pub fn pool(self, p: Vec<Option<u8>>) -> Self { self } }",
            "impl T { pub fn pool(self, p: u8) -> Self { self } }",
            "impl T { pub fn pool(self, p: &Option<u8>) -> Self { self } }",
            "impl T { pub fn pool<O: Into<u8>>(self, p: O) -> Self { self } }",
        ] {
            assert_eq!(rules(ok), vec![], "{ok}");
        }
    }

    const HOSTING: &str =
        "#[derive(Debug, Default)] pub enum Hosting { #[default] Auto, InProcess }\n";

    fn tree_rules(files: &[(&str, &str)]) -> Vec<String> {
        let mut all = vec![(LIST, "")];
        all.extend_from_slice(files);
        let r = check(&Mem::with(&all), &[], 0);
        r.findings
            .into_iter()
            .filter(|l| l.contains("default-enum-arg"))
            .collect()
    }

    #[test]
    fn a_derived_default_enum_taken_by_a_ref_method_is_a_violation() {
        let src =
            format!("{HOSTING}impl S {{ pub fn load(&self, id: u8, h: Hosting) -> u8 {{ 0 }} }}");
        assert_eq!(rules(&src), one("load", "default-enum-arg"));
        for peeled in [
            "h: &Hosting",
            "h: &mut Hosting",
            "h: impl Into<Hosting>",
            "h: crate::Hosting",
        ] {
            let src = format!("{HOSTING}impl S {{ pub fn load(&self, id: u8, {peeled}) {{}} }}");
            assert_eq!(rules(&src), one("load", "default-enum-arg"), "{peeled}");
        }
        let src =
            format!("{HOSTING}impl S {{ pub fn new(id: u8, h: Hosting) -> Self {{ todo!() }} }}");
        assert_eq!(rules(&src), one("new", "default-enum-arg"));
        let src = format!(
            "{HOSTING}impl S {{ pub fn load<H: Into<Hosting>>(&self, id: u8, h: H) {{}}\n\
             pub fn open<H>(&self, id: u8, h: H) where H: Into<Hosting> {{}} }}"
        );
        assert_eq!(
            rules(&src),
            vec![
                ("load".to_owned(), "default-enum-arg"),
                ("open".to_owned(), "default-enum-arg")
            ]
        );
    }

    #[test]
    fn a_hand_default_enum_is_known_in_its_own_crate_only() {
        let en = "pub enum Mode { A, B }\nimpl<T> Default for Mode { fn default() -> Self { Mode::A } }\n";
        let user = "impl S { pub fn go(&self, id: u8, m: Mode) {} }";
        let same = tree_rules(&[("crates/a/src/lib.rs", user), ("crates/a/src/mode.rs", en)]);
        assert_eq!(same.len(), 1, "{same:?}");
        let other = tree_rules(&[("crates/a/src/lib.rs", user), ("crates/b/src/mode.rs", en)]);
        assert!(other.is_empty(), "{other:?}");
        let prefixed = "pub enum Mode { A }\nimpl core::default::Default for Mode { fn default() -> Self { Mode::A } }\n";
        let same = tree_rules(&[
            ("crates/a/src/lib.rs", user),
            ("crates/a/src/mode.rs", prefixed),
        ]);
        assert_eq!(same.len(), 1, "{same:?}");
    }

    #[test]
    fn a_chained_option_taking_a_default_enum_and_the_enums_own_methods_are_not_violations() {
        for ok in [
            "impl C { pub fn hosting(self, id: u8, h: Hosting) -> Self { self } }",
            "impl C { pub fn hosting(mut self, id: u8, h: Hosting) -> C { self } }",
            "impl Hosting { pub fn same(&self, id: u8, h: Hosting) -> bool { true } }",
            "impl C { pub fn hosting(&self, n: u8) {} }",
        ] {
            assert_eq!(rules(&format!("{HOSTING}{ok}")), vec![], "{ok}");
        }
        // A by-value receiver that does not build its owner takes the option positionally.
        let src = format!(
            "{HOSTING}impl C {{ pub fn spawn(self, id: u8, h: Hosting) -> Product {{ todo!() }} }}"
        );
        assert_eq!(rules(&src), one("spawn", "default-enum-arg"));
    }

    #[test]
    fn a_setter_taking_only_the_default_enum_passes_and_with_a_second_parameter_fails() {
        let only = format!(
            "{HOSTING}impl S {{ pub fn set_mode(&mut self, h: Hosting) {{}}\n\
                            pub fn pick(h: Hosting) -> u8 {{ 0 }} }}"
        );
        assert_eq!(rules(&only), one("set_mode", "setter"));
        let two = format!("{HOSTING}impl S {{ pub fn load(&mut self, id: u8, h: Hosting) {{}} }}");
        assert_eq!(rules(&two), one("load", "default-enum-arg"));
    }

    #[test]
    fn a_keyed_setter_sets_its_last_parameter_and_an_enum_before_it_is_an_option() {
        let keyed =
            format!("{HOSTING}impl S {{ pub fn set_mode(&mut self, id: u8, h: Hosting) {{}} }}");
        assert_eq!(rules(&keyed), one("set_mode", "setter"));
        let first =
            format!("{HOSTING}impl S {{ pub fn set_mode(&mut self, h: Hosting, id: u8) {{}} }}");
        let found = rules(&first);
        assert!(
            found.iter().any(|(_, r)| *r == "default-enum-arg"),
            "an enum before the set value is an option: {found:?}"
        );
    }

    #[test]
    fn a_name_resolves_to_the_crate_it_is_imported_from() {
        let en = "#[derive(Default)] pub enum Direction { #[default] Up, Down }";
        let st = "pub struct Direction(f64);";
        let user = |imp: &str, ty: &str| {
            format!("{imp}\nimpl S {{ pub fn go(&self, id: u8, d: {ty}) {{}} }}")
        };
        let run = |imp: &str, ty: &str| {
            tree_rules(&[
                ("crates/tutti-a/src/lib.rs", &user(imp, ty)),
                ("crates/tutti-e/src/lib.rs", en),
                ("crates/tutti-s/src/lib.rs", st),
            ])
            .len()
        };
        assert_eq!(run("use tutti_s::Direction;", "Direction"), 0);
        assert_eq!(run("use tutti_e::Direction;", "Direction"), 1);
        assert_eq!(run("use tutti_e::{Other, Direction};", "Direction"), 1);
        assert_eq!(run("use tutti_s::{a::{Other}, Direction};", "Direction"), 0);
        assert_eq!(run("use tutti_e::module::Direction;", "Direction"), 1);
        assert_eq!(run("use tutti_e;", "tutti_e::Direction"), 1);
        assert_eq!(run("", "tutti_s::Direction"), 0);
        assert_eq!(run("", "tutti_e::Direction"), 1);
        // No import and no path: the file's own crate, which has no `Direction`.
        assert_eq!(run("", "Direction"), 0);
        assert_eq!(run("use std::fmt::Direction;", "Direction"), 0);
    }

    #[test]
    fn an_as_rename_resolves_by_its_local_name() {
        let en = "#[derive(Default)] pub enum Direction { #[default] Up, Down }";
        let st = "pub struct Direction(f64);";
        let run = |imp: &str, ty: &str| {
            let user = format!("{imp}\nimpl S {{ pub fn go(&self, id: u8, d: {ty}) {{}} }}");
            tree_rules(&[
                ("crates/tutti-a/src/lib.rs", &user),
                ("crates/tutti-e/src/lib.rs", en),
                ("crates/tutti-s/src/lib.rs", st),
            ])
            .len()
        };
        assert_eq!(run("use tutti_e::Direction as Heading;", "Heading"), 1);
        assert_eq!(run("use tutti_s::Direction as Heading;", "Heading"), 0);
        // The original name is no longer in scope: it is the file's own crate's.
        assert_eq!(run("use tutti_e::Direction as Heading;", "Direction"), 0);
    }

    #[test]
    fn an_enum_without_a_default_is_not_a_usual_value() {
        let src = "pub enum Kind { A, B }\nimpl S { pub fn go(&self, k: Kind) {} }";
        assert_eq!(rules(src), vec![]);
        let src =
            "#[derive(Debug)] pub enum Kind { A, B }\nimpl S { pub fn go(&self, k: Kind) {} }";
        assert_eq!(rules(src), vec![]);
    }

    #[test]
    fn an_option_arg_row_with_no_violation_is_stale_and_fails() {
        let list = "a  option-arg  crates/a/src/lib.rs::T::pool\n";
        let r = run(
            list,
            "impl T { pub fn pool(self, p: u8) -> Self { self } }",
            &[],
        );
        assert!(!r.passed());
    }

    #[test]
    fn a_setter_on_a_building_owner_is_a_violation_and_a_by_value_or_private_one_is_not() {
        for owner in [
            "GainNode",
            "FileSource",
            "RenderSpec",
            "MixConfig",
            "LoadOptions",
        ] {
            let src = format!(
                "impl {owner} {{ pub fn set_gain(&self, g: f32) {{}}\n\
                               pub fn set_mode(&mut self, m: u8) {{}} }}"
            );
            assert_eq!(
                rules(&src),
                [
                    ("set_gain".to_owned(), "setter"),
                    ("set_mode".to_owned(), "setter")
                ],
                "{owner}"
            );
        }
        let ok = "impl GainNode { pub fn set_gain(self, g: f32) -> Self { self } }\n\
                  impl GainNode { pub fn gain(&self) -> f32 { 0.0 } }\n\
                  impl GainNode { pub(crate) fn set_gain(&self, g: f32) {} }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_setter_on_an_owner_that_is_not_a_node_source_or_spec_is_a_violation() {
        for owner in ["GainHandle", "Transport", "Meter", "Node"] {
            let src = format!("impl {owner} {{ pub fn set_gain(&self, g: f32) {{}} }}");
            assert_eq!(rules(&src), one("set_gain", "setter"), "{owner}");
        }
    }

    #[test]
    fn a_public_constructor_on_a_node_is_a_violation_and_one_on_its_config_is_not() {
        let bad = "impl GainNode { pub fn new(c: GainConfig) -> Self { todo!() }\n\
                   pub fn open(p: &str) -> Result<Self, E> { todo!() }\n\
                   pub fn maybe() -> Option<GainNode> { None } }";
        let got = rules(bad);
        assert!(got.contains(&("new".to_owned(), "node-ctor")));
        assert!(got.contains(&("open".to_owned(), "node-ctor")));
        assert!(got.contains(&("maybe".to_owned(), "node-ctor")));
        let ok = "impl GainNode { pub fn gain(&self) -> Param<Db> { todo!() }\n\
                  pub(crate) fn new() -> Self { todo!() } }\n\
                  impl Other { pub fn new() -> Self { todo!() } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_config_constructor_must_take_a_width_first() {
        let ok = "impl AConfig { pub fn new(l: ChannelLayout, r: u8) -> Self { todo!() } }\n\
                  impl BConfig { pub fn new(t: tutti_buffers::BusTopology) -> Self { todo!() } }\n\
                  impl CConfig { pub fn new(l: impl Into<ChannelLayout>) -> Self { todo!() } }\n\
                  impl DConfig { pub fn new<L: Into<BusTopology>>(l: L) -> Self { todo!() } }\n\
                  impl EConfig { pub fn new<L>(l: L) -> Self where L: Into<ChannelLayout> { todo!() } }\n\
                  impl FConfig { pub fn gain(&self) -> Self { todo!() } }\n\
                  impl From<AConfig> for ANode { fn from(c: AConfig) -> Self { todo!() } }\n\
                  impl From<BConfig> for BNode { fn from(c: BConfig) -> Self { todo!() } }\n\
                  impl From<CConfig> for CNode { fn from(c: CConfig) -> Self { todo!() } }\n\
                  impl From<DConfig> for DNode { fn from(c: DConfig) -> Self { todo!() } }\n\
                  impl From<EConfig> for ENode { fn from(c: EConfig) -> Self { todo!() } }\n\
                  impl From<FConfig> for FNode { fn from(c: FConfig) -> Self { todo!() } }";
        assert!(rules(ok).is_empty());
        for bad in [
            "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }",
            "impl XConfig { pub fn new() -> Self { todo!() } }",
            "impl XConfig { pub fn new(r: u32, l: ChannelLayout) -> Self { todo!() } }",
            "impl XConfig { pub fn new<L: Into<u8>>(l: L) -> Self { todo!() } }",
        ] {
            let node = format!(
                "{bad}\nimpl From<XConfig> for XNode {{ fn from(c: XConfig) -> Self {{ todo!() }} }}"
            );
            assert_eq!(rules(&node), one("new", "config-width"), "{bad}");
        }
    }

    #[test]
    fn a_config_that_no_node_is_converted_from_is_not_read_for_a_width() {
        let bare = "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }";
        assert!(rules(bare).is_empty());
        let other = "impl From<XConfig> for XSettings { fn from(c: XConfig) -> Self { todo!() } }\n\
                     impl From<YConfig> for XNode { fn from(c: YConfig) -> Self { todo!() } }";
        assert!(rules(&format!("{bare}\n{other}")).is_empty());
        let node = "impl From<XConfig> for XNode { fn from(c: XConfig) -> Self { todo!() } }";
        assert_eq!(
            rules(&format!("{bare}\n{node}")),
            one("new", "config-width")
        );
    }

    #[test]
    fn a_node_config_is_a_from_or_try_from_into_a_node_or_an_alias_of_one() {
        let bare = "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }\n";
        for conv in [
            "impl<F: Float> From<XConfig> for a::XNode<F> { fn from(c: XConfig) -> Self { todo!() } }",
            "impl<F: Float> TryFrom<b::XConfig<F>> for XNode<F> { type Error = E; fn try_from(c: XConfig) -> Result<Self, E> { todo!() } }",
            "impl From<XConfig> for XNode { fn from(c: XConfig) -> Self { todo!() } }",
            "pub type XNode<F> = KernelNode<XKernel<F>>;\n\
             impl<F> TryFrom<XConfig> for XNode<F> { type Error = E; fn try_from(c: XConfig) -> Result<Self, E> { todo!() } }",
        ] {
            assert_eq!(
                rules(&format!("{bare}{conv}")),
                one("new", "config-width"),
                "{conv}"
            );
        }
        let across = Mem::with(&[
            (LIST, ""),
            (F, bare),
            (
                "crates/a/src/node.rs",
                "pub type XNode<F> = KernelNode<XKernel<F>>;\n\
                 impl<F> TryFrom<XConfig> for XNode<F> { type Error = E; fn try_from(c: XConfig) -> Result<Self, E> { todo!() } }",
            ),
        ]);
        let r = check(&across, &[], 0);
        assert_eq!(
            r.findings[1],
            "  a  config-width  crates/a/src/lib.rs::XConfig::new"
        );
    }

    #[test]
    fn a_node_constructor_taking_a_config_makes_it_a_node_config() {
        let bare = "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }\n";
        for ctor in [
            "impl XNode { pub fn new(config: XConfig) -> Self { todo!() } }",
            "impl<K> XNode<K> { pub fn new(config: &x::XConfig) -> Self { todo!() } }",
            "impl XNode { pub fn from_config(rate: u32, config: impl Into<XConfig>) -> Self { todo!() } }",
        ] {
            assert_eq!(rules(&format!("{bare}{ctor}")).len(), 2, "{ctor}");
            assert!(rules(&format!("{bare}{ctor}")).contains(&("new".to_owned(), "config-width")));
        }
        let not = "impl XNode { pub fn new(config: YConfig) -> Self { todo!() } }\n\
                   impl XNode { pub fn tuned(&self, config: XConfig) -> Self { todo!() } }\n\
                   impl XOther { pub fn new(config: XConfig) -> Self { todo!() } }";
        assert!(!rules(&format!("{bare}{not}")).contains(&("new".to_owned(), "config-width")));
    }

    #[test]
    fn a_result_constructor_on_a_building_owner_is_a_violation_and_a_plain_one_is_not() {
        let bad = "impl PhaserConfig { pub fn new(r: f32) -> Result<Self, E> { todo!() } }";
        assert!(rules(bad).contains(&("new".to_owned(), "ctor-result")));
        let ok = "impl XSource { pub fn probe(p: &str) -> Result<Report, E> { todo!() } }\n\
                  impl Wave { pub fn plain(p: &str) -> Self { todo!() } }\n\
                  impl Wave { pub fn load(&self) -> Result<Self, E> { todo!() } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_result_constructor_on_an_owner_that_is_not_a_building_one_is_a_violation() {
        for owner in ["Wave", "Session", "Fold"] {
            let src = format!(
                "impl {owner} {{ pub fn open(p: &str) -> Result<{owner}, E> {{ todo!() }} }}"
            );
            assert_eq!(rules(&src), one("open", "ctor-result"), "{owner}");
        }
        let src = "impl Wave { pub fn read(p: &str) -> Result<Self> { todo!() } }";
        assert_eq!(rules(src), one("read", "ctor-result"));
    }

    #[test]
    fn a_macro_writing_a_range_type_is_a_violation_outside_its_homes_only() {
        let src = "macro_rules! ranged { ($n:ident) => { impl $n { pub fn new(raw: u8) -> Option<Self> { todo!() } } }; }\n\
                   macro_rules! plain { ($n:ident) => { impl $n { pub fn new(a: u8, b: u8) -> Option<Self> { todo!() } } }; }";
        assert_eq!(rules(src), one("ranged", "range-macro"));
        assert!(scan("crates/tutti-units/src/lib.rs", src).is_empty());
        assert!(scan("crates/tutti-rt/src/id.rs", src).is_empty());
    }

    #[test]
    fn items_in_tests_comments_strings_and_non_public_fns_are_not_read() {
        let src = "// pub fn with_a(self) -> Self\nconst S: &str = \"impl XNode { pub fn new() -> Self }\";\n\
                   impl T { pub(crate) fn with_b(self) -> Self { self } }\n\
                   #[cfg(test)] mod m { impl T { pub fn with_c(self) -> Self { self } } }\n\
                   trait Q { fn with_d(self) -> Self; }\n";
        assert!(rules(src).is_empty());
    }

    #[test]
    fn a_violation_without_a_row_fails_and_names_the_row_to_write() {
        let src = "impl T { pub fn with_x(self) -> Self { self } }";
        let r = run("", src, &[]);
        assert!(!r.passed());
        assert_eq!(
            r.findings[1],
            "  a  with-prefix  crates/a/src/lib.rs::T::with_x"
        );
    }

    #[test]
    fn a_baselined_violation_passes_with_its_count() {
        let src = "impl T { pub fn with_x(self) -> Self { self } }";
        let r = run("a  with-prefix  crates/a/src/lib.rs::T::with_x\n", src, &[]);
        assert!(r.passed());
        assert_eq!(
            r.out[1],
            "check-building: 1 violations, 1 rows in xtask/building-baseline.txt (a 1)"
        );
    }

    #[test]
    fn a_row_with_no_violation_is_stale_and_fails() {
        let r = run(
            "a  with-prefix  crates/a/src/lib.rs::T::with_x\n",
            "pub fn f() {}",
            &[],
        );
        assert!(!r.passed());
        assert_eq!(
            r.findings[1],
            "  a  with-prefix  crates/a/src/lib.rs::T::with_x"
        );
    }

    #[test]
    fn a_clean_tree_passes() {
        let r = run("", "impl T { pub fn x(self) -> Self { self } }", &[]);
        assert!(r.passed());
        assert_eq!(r.out[0], "check-building: OK");
    }

    #[test]
    fn an_allowed_item_passes_without_a_row_and_a_row_for_it_is_stale() {
        let src = "impl Shape { pub fn with_cost(self, c: u8) -> Self { self } }";
        let allow = [Allow {
            path: "crates/a/",
            owner: "Shape",
            rules: &[],
            reason: "r",
        }];
        assert!(run("", src, &allow).passed());
        let r = run(
            "a  with-prefix  crates/a/src/lib.rs::Shape::with_cost\n",
            src,
            &allow,
        );
        assert!(!r.passed());
        let other = "impl Other { pub fn with_cost(self, c: u8) -> Self { self } }";
        assert!(!run("", other, &allow).passed());
    }

    #[test]
    fn a_config_width_allow_row_covers_its_type_and_not_another_config_of_the_crate() {
        let src = "impl AlignConfig { pub fn new(r: u8) -> Self { todo!() } }\n\
                   impl OtherConfig { pub fn new(r: u8) -> Self { todo!() } }\n\
                   impl From<AlignConfig> for AlignNode { fn from(c: AlignConfig) -> Self { todo!() } }\n\
                   impl From<OtherConfig> for OtherNode { fn from(c: OtherConfig) -> Self { todo!() } }";
        let allow = [ty("crates/a/", "AlignConfig", WIDTH, "r")];
        let r = run("", src, &allow);
        assert!(!r.passed());
        assert_eq!(
            r.findings[1],
            "  a  config-width  crates/a/src/lib.rs::OtherConfig::new"
        );
        assert!(
            run(
                "a  config-width  crates/a/src/lib.rs::OtherConfig::new\n",
                src,
                &allow
            )
            .passed()
        );
    }

    #[test]
    fn a_path_qualified_result_or_option_is_peeled_like_a_bare_one() {
        for ret in [
            "io::Result<Self>",
            "crate::Result<Self>",
            "std::io::Result<Foo>",
        ] {
            let src = format!("impl Foo {{ pub fn open() -> {ret} {{ todo!() }} }}");
            assert_eq!(rules(&src), one("open", "ctor-result"), "{ret}");
        }
        let node = "impl GainNode { pub fn new() -> std::option::Option<Self> { todo!() } }";
        assert_eq!(rules(node), one("new", "node-ctor"));
        let chain = "impl Foo { pub fn depth(self) -> io::Result<Self> { todo!() } }";
        assert_eq!(rules(chain), one("depth", "chain-result"));
    }

    #[test]
    fn a_function_allow_row_allows_one_function_and_leaves_its_sibling_counted() {
        let src = "impl Wave { pub fn read(p: &str) -> Result<Self, E> { todo!() }\n\
                   pub fn from_bytes(b: Vec<u8>) -> Result<Self, E> { todo!() } }";
        let allow = [ty("crates/a/", "Wave::read", CTOR, "r")];
        let r = run("", src, &allow);
        assert_eq!(
            r.findings[1],
            "  a  ctor-result  crates/a/src/lib.rs::Wave::from_bytes"
        );
        assert!(
            run(
                "a  ctor-result  crates/a/src/lib.rs::Wave::from_bytes\n",
                src,
                &allow
            )
            .passed()
        );
        let gone = [ty("crates/a/", "Wave::gone", CTOR, "r")];
        let r = run(
            "a  ctor-result  crates/a/src/lib.rs::Wave::from_bytes\na  ctor-result  crates/a/src/lib.rs::Wave::read\n",
            src,
            &gone,
        );
        assert!(r.findings[0].contains("ALLOW covers no violation"));
    }

    #[test]
    fn an_allow_row_that_covers_no_violation_fails() {
        let src = "impl T { pub fn x(self) -> Self { self } }";
        let idle = [ty("crates/a/", "Gone", CTOR, "r")];
        let r = run("", src, &idle);
        assert!(!r.passed());
        assert!(r.findings[0].contains("ALLOW covers no violation"));
        assert_eq!(r.findings[1], "  crates/a/ Gone: r");
        let wrong_rule = "impl Gone { pub fn with_x(self) -> Self { self } }";
        let r = run(
            "a  with-prefix  crates/a/src/lib.rs::Gone::with_x\n",
            wrong_rule,
            &idle,
        );
        assert!(!r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn an_allow_row_covers_only_its_rules_and_the_ffi_crates_by_name() {
        let src = "impl XConfig { pub fn new(r: u8) -> Result<Self, E> { todo!() } }";
        let one_rule = [Allow {
            path: "crates/a/",
            owner: "",
            rules: &[Rule::ConfigWidth],
            reason: "r",
        }];
        let r = run("", src, &one_rule);
        assert_eq!(
            r.findings[1],
            "  a  ctor-result  crates/a/src/lib.rs::XConfig::new"
        );
        let ffi = [Allow {
            path: FFI,
            owner: "",
            rules: &[],
            reason: "r",
        }];
        let tree = Mem::with(&[
            (LIST, ""),
            (
                "crates/a-ffi/src/lib.rs",
                "impl T { pub fn with_x(self) -> Self { self } }",
            ),
            (
                "crates/b/src/lib.rs",
                "impl T { pub fn with_x(self) -> Self { self } }",
            ),
        ]);
        let r = check(&tree, &ffi, 0);
        assert_eq!(
            r.findings[1],
            "  b  with-prefix  crates/b/src/lib.rs::T::with_x"
        );
    }

    #[test]
    fn a_malformed_or_misfiled_row_fails() {
        for list in [
            "a with-prefix\n",
            "a nonsense crates/a/src/lib.rs::T::f\n",
            "b with-prefix crates/a/src/lib.rs::T::f\n",
            "a with-prefix crates/a/src/lib.rs::T::f\na with-prefix crates/a/src/lib.rs::T::f\n",
        ] {
            let r = run(list, "", &[]);
            assert!(!r.passed(), "{list}");
            assert!(r.findings[0].contains("malformed"));
        }
    }

    #[test]
    fn a_walk_that_finds_too_few_violations_fails() {
        let tree = Mem::with(&[(LIST, ""), (F, "pub fn f() {}")]);
        assert!(check(&tree, &[], 1).findings[0].contains("the walk is broken"));
    }
}
