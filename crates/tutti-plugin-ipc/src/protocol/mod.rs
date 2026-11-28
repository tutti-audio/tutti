//! Wire format for the plugin bridge. Every type here is `Serialize +
//! Deserialize` and flows between host and server process.
//!
//! Non-wire runtime types (`AudioBuffer<T>`, window handles) live in
//! [`tutti_plugin_types`]. Host-local config lives
//! in [`crate::config`].

#[macro_use]
mod message;
pub(crate) mod envelope;
pub(crate) mod frame;
pub(crate) mod process;
pub(crate) mod shm;
pub(crate) mod wire;

/// Wire protocol version, sent by the server in its [`ControlFrame::Hello`].
///
/// Host and subprocess refuse to handshake on a mismatch, so a skew fails at the
/// handshake and never mis-parses a message: the codec (postcard) is not
/// self-describing, a struct is positional and an enum is a varint over declaration
/// order. Any change to a serialized type's shape, an appended field and an
/// appended variant included, moves this.
pub const PROTOCOL_VERSION: u32 = 75;

/// Largest frame body the control connection carries, either way, in bytes.
///
/// The wire is `[u32 big-endian length][postcard payload]`, so the length is
/// **attacker-controlled**: a corrupt or hostile peer can advertise `u32::MAX`,
/// and an unbounded reader would answer with `vec![0u8; len]`, a 4 GiB zeroed
/// allocation made before a single byte of body had been seen. Under Linux
/// overcommit that mapping is lazy and effectively free, which hides the
/// hazard; with `overcommit_memory=2`, a cgroup limit, or a 32-bit host, the
/// allocator fails and Rust's OOM handler **aborts the process**: the whole
/// DAW, not the bridge.
///
/// **This bounds size, and only size.** It does *not* bound how long a read
/// takes: `SO_RCVTIMEO` restarts on every syscall, so any under-cap frame can
/// be paced out one byte at a time. The total-elapsed deadline in
/// `transport::control::read_exact_by` answers that, and the two are
/// independent.
///
/// Each lane and direction has its own cap, carried by its message type
/// ([`Framed`]): the control lane's messages are bounded by a
/// fixed struct or by `ParameterList`, so this is far above an honest control
/// frame and far below a denial of service. **Plugin state is not carried in a
/// frame**: it travels as a memory object beside the frame that declares it,
/// bounded by [`MAX_STATE_BYTES`](tutti_plugin_types::MAX_STATE_BYTES).
pub const CONTROL_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Largest frame body the audio connection takes from the host, in bytes: a
/// block's request, whose automation is megabytes at a large block, a large
/// queue count and the host's point budget.
pub const AUDIO_REQUEST_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Largest frame body the audio connection takes from the server, in bytes: a
/// block's reply with its MIDI at the cap, each event at the most its varints
/// take. The host reads no more than this from the audio connection, so a
/// server cannot make it allocate for a reply bigger than a block's.
pub const AUDIO_REPLY_FRAME_BYTES: usize = tutti_plugin_types::MIDI_STACK_CAPACITY * 32 + 256;

/// Validate a subprocess-reported protocol version against [`PROTOCOL_VERSION`].
/// Called at each handshake consumer so a version skew fails loudly instead of
/// mis-parsing later messages.
///
/// # Errors
///
/// [`IpcError::ProtocolMismatch`](tutti_plugin_types::IpcError::ProtocolMismatch)
/// when the versions differ.
pub(crate) fn check_protocol_version(got: u32) -> tutti_plugin_types::IpcResult<()> {
    if got == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(tutti_plugin_types::IpcError::ProtocolMismatch {
            expected: PROTOCOL_VERSION,
            got,
        })
    }
}

pub(crate) use envelope::default_format;
pub use envelope::{
    AppMidiMapApplied, ApplyOutcome, ArmPluginMidiLearn, AudioReply, AudioRequest, BlockWrites,
    CloseEditor, ControlFrame, ControlReply, ControlRequest, CurrentPreset, EditorOpened,
    EditorSized, GetCurrentPreset, GetParameter, GetParameterList, GetParameterNormalized,
    GetParameterPlain, GetParameterText, GetParameterValueFromText, GetPresetList, GetWiring,
    HostEdit, Lane, LaneOpened, LoadPlugin, LoadPreset, LoadState, ModeOutcome, News, OpenEditor,
    OpenLane, ParameterList, ParameterNormalized, ParameterPlain, ParameterText, ParameterValue,
    ParameterValueFromText, PluginLoaded, PluginMidiLearn, PresetList, PresetLoaded, RenderModeSet,
    ReqId, Request, SampleRateSet, SaveState, SetAppMidiMap, SetAutomationState, SetEditorSize,
    SetRenderMode, SetSampleRate, SetTrackInfo, SetupSharedMemory, SharedMemoryReady, Shutdown,
    StageState, StagedState, StateLoaded, StateReady, StateStaged, Wiring, Working, serve,
};
pub use frame::Framed;
pub use message::{Answered, Handle, Handling, Message, Reaction, SavedState, Stop, Unanswered};
pub use process::ProcessAudioData;
pub use shm::SlabLayout;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tutti_buffers::FloatWidth;
    use tutti_plugin_types::RateRefusalCause;

    use tutti_plugin_types::{
        ChordValue, MidiEventVec, NoteExpressionIntValue, NoteExpressionTextValue, ParameterInfo,
        PluginFormat, PluginId, PluginPreset, PluginTail, PresetError, PresetId, Samples,
        ScaleValue, TransportInfo, Uid, WireMidi,
    };
    use tutti_ump::{CCNumber, ControlValue, Message, MidiChannel, NoteVelocity, U7, Ump, Voice2};
    use tutti_units::{Normalized, ParamId, SampleRate};

    use tutti_fixtures::midi;

    /// **A `PluginIoChanged` with no layout is its variant tag and one `None`
    /// byte, and one with a layout ends in what follows that `Some`.** The
    /// encoding is the wire's: a reordered variant or a changed option is a
    /// protocol change, and this fails until the version moves.
    #[test]
    fn a_plugin_io_changed_without_a_layout_is_two_bytes() {
        let none =
            postcard::to_allocvec(&News::PluginIoChanged { layout: None }).expect("serialize");
        assert_eq!(none.len(), 2, "{none:?}");
        assert_eq!(none[1], 0, "no layout is a zero option byte");
        let some = postcard::to_allocvec(&News::PluginIoChanged {
            layout: Some(tutti_plugin_types::Layout::default()),
        })
        .expect("serialize");
        assert_eq!(some[..2], [none[0], 1], "the same variant, then `Some`");
        assert_eq!(some[2..], [0, 0], "and two empty bus lists");
    }

    /// Every preset frame survives the wire in both directions.
    ///
    /// These are the frames that make presets reachable out-of-process: an id
    /// produced by `PresetList` in the subprocess is handed back through
    /// `LoadPreset`, so a variant that does not round-trip is a preset that can
    /// be listed and never loaded.
    #[test]
    fn every_preset_frame_round_trips() {
        let requests = [
            ControlRequest::GetPresetList,
            ControlRequest::LoadPreset {
                id: PresetId::Number(9000),
            },
            ControlRequest::LoadPreset {
                id: PresetId::Program {
                    list_id: 3,
                    index: 7,
                },
            },
            ControlRequest::LoadPreset {
                id: PresetId::Location(PathBuf::from("/p/lead.clap-preset")),
            },
            ControlRequest::GetCurrentPreset,
        ];
        for msg in requests {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let back: ControlRequest = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }

        let responses = [
            ControlReply::PresetList {
                presets: vec![
                    PluginPreset::new(PresetId::Number(0), "Init"),
                    PluginPreset::in_bank(
                        PresetId::Program {
                            list_id: 1,
                            index: 2,
                        },
                        "Warm",
                        "Bank 1",
                    ),
                ],
            },
            ControlReply::PresetLoaded { result: Ok(()) },
            ControlReply::PresetLoaded {
                result: Err(PresetError::Unsupported),
            },
            ControlReply::PresetLoaded {
                result: Err(PresetError::Unlisted),
            },
            ControlReply::PresetLoaded {
                result: Err(PresetError::Refused),
            },
            ControlReply::CurrentPreset {
                id: Some(PresetId::Number(4)),
            },
            ControlReply::CurrentPreset { id: None },
        ];
        for msg in responses {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let back: ControlReply = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }
    }

    /// Every parameter-display frame survives the wire in both directions.
    ///
    /// These are what make a plugin's own value text reachable out-of-process.
    /// `None` is carried explicitly in both replies: it is the answer for a
    /// plugin that declined, and a caller renders the raw number on it — so a
    /// variant that dropped the `Option` would turn "did not answer" into an
    /// empty label.
    #[test]
    fn every_parameter_display_frame_round_trips() {
        let requests = [
            ControlRequest::GetParameterText {
                param_id: ParamId::new(0x8000_0001),
                value: Normalized::of(0.375),
            },
            // The other addressing model, which VST2 uses.
            ControlRequest::GetParameterText {
                param_id: ParamId::new(3),
                value: Normalized::of(1.0),
            },
            ControlRequest::GetParameterValueFromText {
                param_id: ParamId::new(7),
                text: "800 Hz".to_string(),
            },
            ControlRequest::GetParameterValueFromText {
                param_id: ParamId::new(0),
                text: String::new(),
            },
        ];
        for msg in requests {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let back: ControlRequest = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }

        let responses = [
            ControlReply::ParameterText {
                text: Some("Bandpass".to_string()),
            },
            ControlReply::ParameterText { text: None },
            ControlReply::ParameterValueFromText {
                value: Some(Normalized::of(0.375)),
            },
            ControlReply::ParameterValueFromText { value: None },
            ControlReply::SampleRateSet {
                rate: SampleRate::SR_96K,
                refusal: None,
            },
            ControlReply::SampleRateSet {
                rate: SampleRate::SR_96K,
                refusal: Some(RateRefusalCause::Deactivated {
                    reason: "refused".to_string(),
                }),
            },
        ];
        for msg in responses {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let back: ControlReply = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }
    }

    /// Both conversion frames, request and reply, survive the wire in both
    /// directions, with the `None` of a format that has no plain value carried
    /// explicitly and the plain value at full `f64` width (an `f32` would turn
    /// 632.4555320336758 into 632.4555053710938).
    #[test]
    fn every_parameter_conversion_frame_round_trips() {
        let requests = [
            ControlRequest::GetParameterPlain {
                param_id: ParamId::new(3),
                value: Normalized::of(0.375),
            },
            ControlRequest::GetParameterNormalized {
                param_id: ParamId::new(3),
                plain: 632.455_532_033_675_8,
            },
        ];
        for msg in requests {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let back: ControlRequest = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(format!("{back:?}"), format!("{msg:?}"), "{msg:?}");
        }
        let replies = [
            ControlReply::ParameterPlain {
                plain: Some(632.455_532_033_675_8),
            },
            ControlReply::ParameterPlain { plain: None },
            ControlReply::ParameterNormalized {
                value: Some(Normalized::of(0.5)),
            },
            ControlReply::ParameterNormalized { value: None },
        ];
        for msg in replies {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let back: ControlReply = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(format!("{back:?}"), format!("{msg:?}"), "{msg:?}");
        }
    }

    /// **The audio, news and frame enums' tags are their declaration order, pinned
    /// here as literals.** The control messages' are pinned in
    /// `every_control_request_is_pinned_to_its_row` and
    /// `every_control_reply_is_pinned_to_its_row`.
    ///
    /// postcard encodes an enum as a leading varint discriminant over
    /// declaration order, so inserting a variant mid-enum silently renumbers
    /// every later one, and a symmetric round trip cannot catch it because both
    /// ends move together. The two ends are not one build: a host and its
    /// server negotiate `PROTOCOL_VERSION` and then trust each other's bytes. A
    /// change of order fails here until the version moves and the pins with it.
    #[test]
    fn the_wire_tags_are_the_declaration_order() {
        let tag = |bytes: Vec<u8>| {
            assert!(bytes[0] < 0x80, "a one-byte varint tag");
            bytes[0]
        };
        let block = ProcessAudioData::empty(SampleRate::SR_48K);
        assert_eq!(
            tag(postcard::to_allocvec(&AudioRequest::ProcessAudio(block)).unwrap()),
            0
        );
        assert_eq!(tag(postcard::to_allocvec(&AudioRequest::Reset).unwrap()), 2);
        assert_eq!(
            tag(postcard::to_allocvec(&AudioReply::AudioProcessed {
                latency_us: 0,
                seq: 0,
                midi_out: Default::default(),
                status: Default::default(),
                refused: false,
                writes: crate::BlockWrites::Handed,
            })
            .unwrap()),
            0
        );
        assert_eq!(
            tag(postcard::to_allocvec(&AudioReply::ParametersApplied {
                id: ReqId::new(1),
                outcome: ApplyOutcome::Applied
            })
            .unwrap()),
            1
        );

        let news = |m: News| tag(postcard::to_allocvec(&m).unwrap());
        assert_eq!(news(News::PluginParamValuesChanged), 3);
        assert_eq!(news(News::PluginReloaded), 6);
        assert_eq!(news(News::EditorClosed), 8);
        assert_eq!(
            news(News::PluginStateNotRestored {
                reason: String::new()
            }),
            11
        );
        assert_eq!(
            news(News::PluginWouldNotStop {
                reason: String::new()
            }),
            12
        );
        assert_eq!(
            news(News::ServerLosses {
                commands_refused: 0,
                edits_refused: 0,
                refused_off_thread_calls: tutti_units::Calls::ZERO,
                detached_tasks: 0,
                lanes_lost: 0
            }),
            13
        );
        assert_eq!(news(News::ProcessRequested), 9);

        let frame = |m: ControlFrame| tag(postcard::to_allocvec(&m).unwrap());
        assert_eq!(
            frame(ControlFrame::Reply {
                id: ReqId::new(1),
                reply: ControlReply::SharedMemoryReady
            }),
            0
        );
        assert_eq!(frame(ControlFrame::News(News::EditorClosed)), 1);
        assert_eq!(
            frame(ControlFrame::Working {
                id: ReqId::new(1),
                stage: Working::Opening
            }),
            2
        );
        assert_eq!(
            frame(ControlFrame::Hello {
                protocol_version: 1
            }),
            3
        );

        let outcome = |m: ModeOutcome| tag(postcard::to_allocvec(&m).unwrap());
        assert_eq!(outcome(ModeOutcome::Took), 0);
        assert_eq!(outcome(ModeOutcome::Refused), 1);
        assert_eq!(outcome(ModeOutcome::Unsupported), 2);
    }

    /// One request per row of the message table, named, in the table's order.
    fn control_requests() -> Vec<(&'static str, ControlRequest)> {
        use dpi::{LogicalSize, Size};
        use tutti_plugin_ipc_ffi::{Blob, LaneWire};
        use tutti_plugin_types::{
            AutomationMode, ChannelLayout, ContentScale, RenderMode, StateUse, TrackInfo,
        };

        let layout = SlabLayout {
            samples_per_channel: 8,
            format: FloatWidth::F32,
            slots: 2,
            inputs: vec![ChannelLayout::MONO],
            outputs: vec![ChannelLayout::MONO],
        };
        let (_slab, handle) = crate::AudioSlab::create(layout.clone()).unwrap();
        let blob = || Blob::create(&[]).unwrap().wire();
        let param_id = ParamId::new(3);
        let value = Normalized::of(0.5);
        vec![
            (
                "LoadPlugin",
                ControlRequest::LoadPlugin {
                    path: PathBuf::from("a"),
                    format: PluginFormat::Vst3,
                    plugin: None,
                    sample_rate: SampleRate::SR_48K,
                    block_size: 64,
                    preferred_format: FloatWidth::F32,
                    state: None,
                },
            ),
            (
                "SetAutomationState",
                ControlRequest::SetAutomationState {
                    mode: AutomationMode::default(),
                },
            ),
            ("GetParameter", ControlRequest::GetParameter { param_id }),
            ("GetParameterList", ControlRequest::GetParameterList),
            (
                "SetSampleRate",
                ControlRequest::SetSampleRate {
                    rate: SampleRate::SR_48K,
                },
            ),
            (
                "SaveState",
                ControlRequest::SaveState {
                    state_use: StateUse::Project,
                },
            ),
            (
                "LoadState",
                ControlRequest::LoadState {
                    state_use: StateUse::Project,
                    len: 0,
                    blob: blob(),
                },
            ),
            (
                "SetupSharedMemory",
                ControlRequest::SetupSharedMemory {
                    slab: handle.wire(),
                    layout,
                },
            ),
            ("Shutdown", ControlRequest::Shutdown),
            ("GetPresetList", ControlRequest::GetPresetList),
            (
                "LoadPreset",
                ControlRequest::LoadPreset {
                    id: PresetId::Number(1),
                },
            ),
            ("GetCurrentPreset", ControlRequest::GetCurrentPreset),
            (
                "GetParameterText",
                ControlRequest::GetParameterText { param_id, value },
            ),
            (
                "GetParameterValueFromText",
                ControlRequest::GetParameterValueFromText {
                    param_id,
                    text: String::new(),
                },
            ),
            (
                "StageState",
                ControlRequest::StageState {
                    len: 0,
                    blob: blob(),
                },
            ),
            (
                "GetParameterPlain",
                ControlRequest::GetParameterPlain { param_id, value },
            ),
            (
                "GetParameterNormalized",
                ControlRequest::GetParameterNormalized {
                    param_id,
                    plain: 0.0,
                },
            ),
            (
                "OpenEditor",
                ControlRequest::OpenEditor {
                    parent: None,
                    scale: ContentScale::IDENTITY,
                },
            ),
            ("CloseEditor", ControlRequest::CloseEditor),
            (
                "SetEditorSize",
                ControlRequest::SetEditorSize {
                    size: Size::Logical(LogicalSize::new(1.0, 1.0)),
                },
            ),
            (
                "SetTrackInfo",
                ControlRequest::SetTrackInfo {
                    info: TrackInfo::default(),
                },
            ),
            (
                "OpenLane",
                ControlRequest::OpenLane {
                    lane: Lane::Audio,
                    wire: LaneWire::fresh().unwrap(),
                },
            ),
            (
                "SetAppMidiMap",
                ControlRequest::SetAppMidiMap { app: vec![] },
            ),
            (
                "ArmPluginMidiLearn",
                ControlRequest::ArmPluginMidiLearn { armed: true },
            ),
            ("GetWiring", ControlRequest::GetWiring),
            (
                "SetRenderMode",
                ControlRequest::SetRenderMode {
                    mode: RenderMode::Realtime,
                },
            ),
            (
                "HostEdit",
                ControlRequest::HostEdit {
                    edit: tutti_plugin_types::ParamEdit::Begin(param_id),
                },
            ),
        ]
    }

    /// One reply per row of the message table that has one, named, in the table's
    /// order, and the failure last.
    fn control_replies() -> Vec<(&'static str, ControlReply)> {
        use dpi::{LogicalSize, Size};
        use tutti_plugin_ipc_ffi::Blob;
        use tutti_plugin_types::{
            MidiMapApplied, PluginClass, PluginDescriptor, PluginWiring, ServerError,
        };

        let value = Some(Normalized::of(0.5));
        vec![
            (
                "LoadPlugin",
                ControlReply::PluginLoaded {
                    id: PluginId::Vst3(Uid::new([0; 16])),
                    descriptor: Box::new(PluginDescriptor::new("", PluginClass::Unknown)),
                    loaded: Box::default(),
                    negotiated_format: FloatWidth::F32,
                    state_error: None,
                },
            ),
            ("GetParameter", ControlReply::ParameterValue { value }),
            (
                "GetParameterList",
                ControlReply::ParameterList { parameters: vec![] },
            ),
            (
                "SetSampleRate",
                ControlReply::SampleRateSet {
                    rate: SampleRate::SR_48K,
                    refusal: None,
                },
            ),
            (
                "SaveState",
                ControlReply::StateReady {
                    len: 0,
                    blob: Blob::create(&[]).unwrap().wire(),
                    pending: vec![],
                },
            ),
            ("LoadState", ControlReply::StateLoaded { error: None }),
            ("SetupSharedMemory", ControlReply::SharedMemoryReady),
            (
                "GetPresetList",
                ControlReply::PresetList { presets: vec![] },
            ),
            ("LoadPreset", ControlReply::PresetLoaded { result: Ok(()) }),
            ("GetCurrentPreset", ControlReply::CurrentPreset { id: None }),
            (
                "GetParameterText",
                ControlReply::ParameterText { text: None },
            ),
            (
                "GetParameterValueFromText",
                ControlReply::ParameterValueFromText { value },
            ),
            ("StageState", ControlReply::StateStaged { error: None }),
            (
                "GetParameterPlain",
                ControlReply::ParameterPlain { plain: None },
            ),
            (
                "GetParameterNormalized",
                ControlReply::ParameterNormalized { value },
            ),
            (
                "OpenEditor",
                ControlReply::EditorOpened { result: Ok(None) },
            ),
            (
                "SetEditorSize",
                ControlReply::EditorSized {
                    result: Ok(Size::Logical(LogicalSize::new(1.0, 1.0))),
                },
            ),
            ("OpenLane", ControlReply::LaneOpened),
            (
                "SetAppMidiMap",
                ControlReply::AppMidiMapApplied {
                    applied: MidiMapApplied::Host,
                },
            ),
            (
                "ArmPluginMidiLearn",
                ControlReply::PluginMidiLearn { result: Ok(()) },
            ),
            (
                "GetWiring",
                ControlReply::Wiring {
                    wiring: PluginWiring::default(),
                },
            ),
            (
                "SetRenderMode",
                ControlReply::RenderModeSet {
                    mode: tutti_plugin_types::RenderMode::Realtime,
                    outcome: ModeOutcome::Took,
                },
            ),
            (
                "Error",
                ControlReply::Error {
                    error: ServerError::StateWithoutObject,
                },
            ),
        ]
    }

    /// **A control request's tag is its row's place in the message table, and
    /// every row is pinned here.** postcard writes a varint over declaration
    /// order, so a reordered or inserted row renumbers every later request,
    /// and a round trip cannot see it because both ends move together. The list
    /// above is the expected order as literals; a row added to the table fails
    /// the count until it is added to the list.
    #[test]
    fn every_control_request_is_pinned_to_its_row() {
        let requests = control_requests();
        assert_eq!(requests.len(), ControlRequest::VARIANTS, "a row has no pin");
        for (row, (name, msg)) in requests.iter().enumerate() {
            let bytes = postcard::to_allocvec(msg).expect("serialize");
            assert_eq!(usize::from(bytes[0]), row, "{name}");
            assert!(bytes[0] < 0x80, "{name}: a one-byte varint tag");
            assert!(format!("{msg:?}").starts_with(name), "{name}: {msg:?}");
        }
    }

    /// **Each control reply's tag is pinned, and the failure is the last.**
    #[test]
    fn every_control_reply_is_pinned_to_its_row() {
        let replies = control_replies();
        assert_eq!(replies.len(), ControlReply::VARIANTS, "a row has no pin");
        for (tag, (name, msg)) in replies.iter().enumerate() {
            let bytes = postcard::to_allocvec(msg).expect("serialize");
            assert_eq!(usize::from(bytes[0]), tag, "{name}");
        }
        assert_eq!(replies.last().map(|(name, _)| *name), Some("Error"));
    }

    /// **A frame that lost its last byte is refused**: no field is made up for a
    /// peer that did not send it.
    #[test]
    fn a_truncated_control_frame_is_refused() {
        for (name, request) in control_requests() {
            let bytes = postcard::to_allocvec(&request).expect("serialize");
            let cut = &bytes[..bytes.len() - 1];
            assert!(
                postcard::from_bytes::<ControlRequest>(cut).is_err(),
                "{name}"
            );
        }
        for (name, reply) in control_replies() {
            let bytes = postcard::to_allocvec(&reply).expect("serialize");
            let cut = &bytes[..bytes.len() - 1];
            assert!(postcard::from_bytes::<ControlReply>(cut).is_err(), "{name}");
        }
    }

    /// **A request is its tag, then its fields as postcard writes them, in the order
    /// the row declares them**: a varint tag, a `u32` as a varint, an `f64` as eight
    /// little-endian bytes, a `bool` as one byte, a string as a varint length and
    /// its bytes. Pinned from the declared layout, not from a build's output.
    #[test]
    fn a_request_is_its_tag_then_its_declared_fields() {
        let bytes = |m: ControlRequest| postcard::to_allocvec(&m).expect("serialize");
        let param_id = ParamId::new(3);
        assert_eq!(
            bytes(ControlRequest::GetParameter { param_id }),
            [2, 3],
            "GetParameter is row 2: `param_id`"
        );
        assert_eq!(
            bytes(ControlRequest::GetParameterValueFromText {
                param_id,
                text: "ab".into(),
            }),
            [13, 3, 2, b'a', b'b'],
            "row 13: `param_id`, `text`"
        );
        assert_eq!(
            bytes(ControlRequest::GetParameterNormalized {
                param_id,
                plain: 0.5,
            }),
            [16, 3, 0, 0, 0, 0, 0, 0, 0xE0, 0x3F],
            "row 16: `param_id`, `plain`"
        );
        assert_eq!(
            bytes(ControlRequest::ArmPluginMidiLearn { armed: true }),
            [23, 1],
            "row 23: `armed`"
        );
        assert_eq!(bytes(ControlRequest::GetWiring), [24], "row 24: no fields");
    }

    fn is_note_on(ev: &WireMidi) -> bool {
        matches!(
            ev.ump.decode(),
            Ok(Message::Midi2(_, Voice2::NoteOn { .. }))
        )
    }

    fn note_number(ev: &WireMidi) -> Option<u8> {
        match ev.ump.decode().ok()? {
            Message::Midi2(_, Voice2::NoteOn { note, .. } | Voice2::NoteOff { note, .. }) => {
                Some(note.get())
            }
            _ => None,
        }
    }

    fn at(offset: u32, ump: Ump) -> WireMidi {
        WireMidi { offset, ump }
    }

    #[test]
    fn audio_processed_round_trips_midi_out() {
        // The plugin's MIDI-out must survive the AudioProcessed reply wire trip.
        let midi_out: MidiEventVec = [
            at(
                0,
                midi::note_on(
                    MidiChannel::FIRST,
                    60,
                    NoteVelocity::from(U7::of(100)).get(),
                ),
            ),
            at(
                64,
                midi::cc(
                    MidiChannel::FIRST,
                    CCNumber::VOLUME,
                    ControlValue::from(U7::of(64)).get(),
                ),
            ),
        ]
        .into_iter()
        .collect();
        let msg = AudioReply::AudioProcessed {
            latency_us: 42,
            seq: 7,
            midi_out,
            status: tutti_plugin_types::BlockStatus::Sleep,
            refused: true,
            writes: crate::BlockWrites::Handed,
        };
        let bytes = postcard::to_allocvec(&msg).unwrap();
        let back: AudioReply = postcard::from_bytes(&bytes).unwrap();
        match back {
            AudioReply::AudioProcessed {
                latency_us,
                seq,
                midi_out,
                status,
                refused,
                writes,
            } => {
                assert!(refused);
                assert_eq!(writes, crate::BlockWrites::Handed);
                assert_eq!(latency_us, 42);
                assert_eq!(status, tutti_plugin_types::BlockStatus::Sleep);
                // The block-identity echo. Not load-bearing for audio
                // validity — the slab's per-slot sequence answers that — but it
                // must survive the wire trip for diagnostics to mean anything.
                assert_eq!(seq, 7);
                assert_eq!(midi_out.len(), 2);
                let events = midi_out;
                assert_eq!(events[0].offset, 0);
                assert!(is_note_on(&events[0]));
                assert_eq!(events[1].offset, 64);
            }
            _ => panic!("wrong message type"),
        }
    }

    /// Every `TailChanged` arm survives the wire distinctly.
    ///
    /// The runtime signal is worth no more than the arm it carries: a
    /// plugin that raises its decay to "unbounded" and a plugin that drops
    /// to no tail at all must not arrive as the same message. This is the
    /// `PluginWiring.tail` wire test one level up, on the *update* path —
    /// the load-time field being round-trip-safe says nothing about a
    /// separately-encoded enum variant.
    #[test]
    fn every_tail_change_arm_survives_the_wire() {
        for tail in [
            PluginTail::Unknown,
            PluginTail::None,
            PluginTail::Finite(Samples::new(96_000)),
            PluginTail::Unbounded,
        ] {
            let bytes = postcard::to_allocvec(&News::TailChanged { tail }).unwrap();
            match postcard::from_bytes::<News>(&bytes).unwrap() {
                News::TailChanged { tail: back } => assert_eq!(back, tail),
                other => panic!("expected TailChanged, got {other:?}"),
            }
        }
    }

    #[test]
    fn protocol_version_matches_and_mismatches() {
        assert!(super::check_protocol_version(super::PROTOCOL_VERSION).is_ok());
        // A skew (0 = an ancient binary predating the version field) is rejected.
        assert!(super::check_protocol_version(0).is_err());
        assert!(super::check_protocol_version(super::PROTOCOL_VERSION + 1).is_err());
    }

    /// **`AudioProcessed` encodes its MIDI-out as a sequence**: the variant
    /// index, `latency_us`, `seq`, then a length and each event as
    /// `offset` and four data words, each integer a postcard varint (seven
    /// bits a byte, low group first), then the block's `status` (the
    /// `BlockStatus` variant index: 2 is *sleep*) and `refused`. The bytes are
    /// written out, so a layout change fails here instead of as a desync
    /// between two binaries built apart.
    #[test]
    fn audio_processed_midi_out_has_pinned_bytes() {
        #[rustfmt::skip]
        const BYTES: [u8; 28] = [
            0, // variant
            42, // latency_us
            7, // seq
            2, // two events
            5, 0xC0, 0xF8, 0xC0, 0x84, 0x02, 0, 0, 0, // offset 5, 0x2090_3C40, 0, 0, 0
            0x84, 0x86, 0x88, 0x08, 1, 2, 3, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, // 0x0102_0304; 1, 2, 3, u32::MAX
            2, // status: sleep
            1, // refused
            1, // writes: handed
        ];
        let midi_out: MidiEventVec = [
            WireMidi {
                offset: 5,
                ump: Ump([0x2090_3C40, 0, 0, 0]),
            },
            WireMidi {
                offset: 0x0102_0304,
                ump: Ump([1, 2, 3, 0xFFFF_FFFF]),
            },
        ]
        .into_iter()
        .collect();
        let msg = AudioReply::AudioProcessed {
            latency_us: 42,
            seq: 7,
            midi_out,
            status: tutti_plugin_types::BlockStatus::Sleep,
            refused: true,
            writes: crate::BlockWrites::Handed,
        };
        assert_eq!(postcard::to_allocvec(&msg).unwrap(), BYTES);
        let back: AudioReply = postcard::from_bytes(&BYTES).unwrap();
        assert_eq!(postcard::to_allocvec(&back).unwrap(), BYTES);
    }

    #[test]
    fn test_midi_message_serialization() {
        let midi_events: MidiEventVec = [
            at(
                0,
                midi::note_on(
                    MidiChannel::FIRST,
                    60,
                    NoteVelocity::from(U7::of(100)).get(),
                ),
            ),
            at(
                128,
                midi::note_on(
                    MidiChannel::FIRST,
                    64,
                    NoteVelocity::from(U7::of(100)).get(),
                ),
            ),
            at(
                256,
                midi::cc(
                    MidiChannel::FIRST,
                    CCNumber::VOLUME,
                    ControlValue::from(U7::of(64)).get(),
                ),
            ),
        ]
        .into_iter()
        .collect();

        let msg = AudioRequest::ProcessAudio(ProcessAudioData {
            seq: 42,
            num_samples: 512,
            midi_events,
            ..ProcessAudioData::empty(tutti_plugin_types::SampleRate::SR_48K)
        });

        let encoded = postcard::to_allocvec(&msg).unwrap();
        let decoded: AudioRequest = postcard::from_bytes(&encoded).unwrap();

        match decoded {
            AudioRequest::ProcessAudio(data) => {
                assert_eq!(data.seq, 42);
                assert_eq!(data.num_samples, 512);
                assert_eq!(data.midi_events.len(), 3);

                let events = data.midi_events;
                assert_eq!(events.len(), 3);
                assert_eq!(events[0].offset, 0);
                assert!(is_note_on(&events[0]));
                assert_eq!(note_number(&events[0]), Some(60));
                assert_eq!(events[1].offset, 128);
                assert!(is_note_on(&events[1]));
                assert_eq!(note_number(&events[1]), Some(64));
                assert_eq!(events[2].offset, 256);
            }
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_load_plugin_f64_serialization() {
        let msg = ControlRequest::LoadPlugin {
            path: PathBuf::from("/test/reverb.so"),
            format: PluginFormat::Vst3,
            plugin: Some(PluginId::Vst3(Uid::new([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF,
            ]))),
            sample_rate: SampleRate::SR_96K,
            block_size: 1024,
            preferred_format: FloatWidth::F64,
            state: None,
        };

        let encoded = postcard::to_allocvec(&msg).unwrap();
        let decoded: ControlRequest = postcard::from_bytes(&encoded).unwrap();

        match decoded {
            ControlRequest::LoadPlugin {
                path,
                format,
                plugin,
                sample_rate,
                block_size,
                preferred_format,
                ..
            } => {
                assert_eq!(path, PathBuf::from("/test/reverb.so"));
                assert_eq!(
                    format,
                    PluginFormat::Vst3,
                    "the format crosses the wire, whatever the path's extension"
                );
                assert_eq!(
                    plugin.map(|id| id.to_string()).as_deref(),
                    Some("vst3.00112233-44556677-8899AABB-CCDDEEFF"),
                    "the choice crosses the wire"
                );
                assert_eq!(sample_rate, SampleRate::SR_96K);
                assert_eq!(block_size, 1024);
                assert_eq!(preferred_format, FloatWidth::F64);
            }
            _ => panic!("Wrong message type"),
        }
    }

    /// A width travels as its variant index, 0 for `F32` and 1 for `F64`: the
    /// bytes a version-45 peer wrote, so the width type did not change the wire.
    #[test]
    fn a_float_width_travels_as_its_index() {
        for (width, index) in [(FloatWidth::F32, 0u8), (FloatWidth::F64, 1)] {
            assert_eq!(postcard::to_allocvec(&width).expect("serialize"), [index]);
        }
    }

    #[test]
    fn a_new_transport_info_is_stopped_at_120_in_four_four() {
        let info = TransportInfo::new(SampleRate::SR_48K);
        assert_eq!(info.sample_rate, SampleRate::SR_48K);
        assert_eq!(info.timing.tempo, tutti_units::Bpm::of(120.0));
        // 4/4 — the musical default, now expressed as the type's own default.
        assert_eq!(u32::from(info.timing.signature.beats_per_bar()), 4);
        assert_eq!(u32::from(info.timing.signature.note_value()), 4);
        assert!(!info.state.playing);
        assert!(!info.state.recording);
        assert!(info.loop_region.is_none());
    }

    /// The harmony inputs (chord / scale / text / int) must survive a postcard
    /// round-trip inside `ProcessAudioData`, including the owned `String`
    /// names.
    #[test]
    fn process_audio_round_trips_harmony_fields() {
        let mut data = ProcessAudioData {
            num_samples: 256,
            ..ProcessAudioData::empty(tutti_plugin_types::SampleRate::SR_48K)
        };
        assert!(data.chords.add_change(ChordValue {
            sample_offset: 0,
            root: 60,
            bass_note: 48,
            mask: 0b1001,
            text: "Cmaj7".to_string(),
        }));
        assert!(data.scales.add_change(ScaleValue {
            sample_offset: 0,
            root: 62,
            mask: 0x5ab5,
            text: "D Dorian".to_string(),
        }));
        assert!(data.expr_texts.add_change(NoteExpressionTextValue {
            sample_offset: 4,
            note_id: 7,
            type_id: 1,
            text: "staccato".to_string(),
        }));
        assert!(data.expr_ints.add_change(NoteExpressionIntValue {
            sample_offset: 8,
            note_id: 7,
            type_id: 2,
            value: -42,
        }));

        let bytes = postcard::to_allocvec(&data).unwrap();
        let back: ProcessAudioData = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(back.num_samples, 256);
        assert_eq!(back.chords.changes[0].text, "Cmaj7");
        assert_eq!(back.chords.changes[0].root, 60);
        assert_eq!(back.scales.changes[0].text, "D Dorian");
        assert_eq!(back.expr_texts.changes[0].text, "staccato");
        assert_eq!(back.expr_ints.changes[0].value, -42);
    }

    /// **A parameter's typed unit, role and overview mark cross the wire**, in a
    /// `ParameterList`, and an untyped one stays untyped.
    #[test]
    fn a_parameters_unit_role_and_overview_cross_the_wire() {
        use tutti_plugin_types::{Axis, ControlRole, ParamUnit};

        let typed = ParameterInfo::new(ParamId::new(2), "Pan")
            .with_plain_range(-1.0, 1.0, 0.0)
            .with_measures(Some(ParamUnit::Pan))
            .with_role(Some(ControlRole::Pan(Axis::Y)))
            .with_overview(true)
            .with_short_name("Pn");
        let untyped = ParameterInfo::new(ParamId::new(3), "Other");

        let list = ControlReply::ParameterList {
            parameters: vec![typed.clone(), untyped],
        };
        let ControlReply::ParameterList { parameters } =
            postcard::from_bytes(&postcard::to_allocvec(&list).unwrap()).unwrap()
        else {
            panic!("a ParameterList decodes as one");
        };
        assert_eq!(parameters[0].measures(), Some(ParamUnit::Pan));
        assert_eq!(parameters[0].role(), Some(ControlRole::Pan(Axis::Y)));
        assert!(parameters[0].overview());
        assert_eq!(parameters[0].short_name(), "Pn");
        assert_eq!(
            (
                parameters[1].measures(),
                parameters[1].role(),
                parameters[1].overview()
            ),
            (None, None, false)
        );
    }
}
