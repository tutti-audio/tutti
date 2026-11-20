//! Wire format for the plugin bridge. Every type here is `Serialize +
//! Deserialize` and flows between host and server process.
//!
//! Non-wire runtime types (`AudioBuffer<T>`, window handles) live in
//! [`tutti_plugin_types`]. Host-local config lives
//! in [`crate::config`].

pub(crate) mod envelope;
pub(crate) mod frame;
pub(crate) mod process;
pub(crate) mod shm;
pub(crate) mod wire;

/// Wire protocol version, exchanged in the [`ControlFrame::Ready`] handshake.
///
/// Bump on ANY change to the host↔server wire shape (a new or reordered field on
/// a serialized type), since the wire's codec (postcard) is not self-describing
/// and a skew mis-parses silently. Host and subprocess refuse to handshake on a mismatch.
///
/// # Why an appended field is still a bump
///
/// Two kinds of change are easy to mistake for compatible, and neither is:
///
/// - A **struct** is positional: fields are written in declaration order with no
///   tags, so a peer one version behind stops reading before the new field, and
///   a payload from the newer side runs the decoder off the end or into the next
///   field's bytes. `serde(default)` does not rescue this — it covers the JSON
///   and struct-update paths, not this wire.
/// - **Version 24** appended `ParameterInfo::measures` (an `Option<ParamUnit>`, a
///   variant index) and `role` (an `Option<u16>`, a `UnitParam` discriminant)
///   to every entry of a `ParameterList`: two
///   `0x00` bytes after `group` when both are `None`. No recorded wire figure
///   holds a `ParameterInfo`.
/// - An **enum variant** is a varint discriminant over declaration order, so a
///   peer receiving a tag it has no arm for fails the decode mid-stream rather
///   than at a message boundary. Removing one renumbers every variant after it:
///   **version 25** removed `OpenEditor`, `CloseEditor`, `EditorOpened` and
///   `EditorClosed`, which nothing sent (the host opens an editor in its own
///   process, through its `EditorLoader`).
/// - A **field's type** is its bytes too: **version 26** made `LoadedPlugin`'s
///   `inputs` and `outputs` lists of `PluginBus` (width, placement, a role
///   byte) and dropped the `input_topology` and `output_topology` lists that
///   followed `probed`, so a load reply reads differently from its first byte.
/// - The **codec** is the whole wire: **version 27** moved it from bincode 1
///   (unmaintained, RUSTSEC-2025-0141) to postcard, whose integers and enum
///   tags are varints, so every message reads differently. A version-26 server
///   is refused at the handshake (`handshake` names a server whose first frame
///   does not decode as one from another build).
/// - **Version 28** added `load` to `LoadPlugin`: which plugin of a file that
///   exports several to load (a CLAP plugin id, a VST3 class id, an AU
///   descriptor id), which the server hands its format's
///   `OutOfProcess::load_served`. Before it a file always loaded its first
///   plugin.
/// - **Version 29** added `seconds` to the transport's `LoopRegion` (the loop's
///   bounds in seconds from the project's tempo map, which CLAP's
///   `loop_start_seconds` / `loop_end_seconds` carry), a field of every
///   `TransportInfo` on the wire.
/// - **Version 30** added `GetParameterPlain` and `GetParameterNormalized` (and
///   their replies, `ParameterPlain` and `ParameterNormalized`): the conversion
///   between a normalized value and the plugin's plain one is the format's, so
///   it is a request to the server, whose format answers it.
/// - **Version 31** carries parameter values as `f64`: `SetParameter`,
///   `ParameterValue` and `ParameterChanged` held an `f32` (so 0.3 came back
///   as 0.30000001192...), while automation points and text queries were
///   already `f64`. It also replaces `ParameterChanged`'s `index` and `value`
///   with the shared `ParamEdit` (the id, the value, the gesture it falls in
///   and whether to record it), so the server forwards a plugin's edits, a
///   gesture's begin and end among them.
///
/// - **Version 32** adds `status` to `AudioProcessed`: what the plugin said of
///   itself at the end of the block (`BlockStatus`: CLAP's process status as
///   *continue*, *tail* or *sleep*), so the node can report no tail and park
///   when the plugin sleeps. It is a field of every block's
///   reply, so a version-31 server's replies read differently from the first
///   byte after the MIDI.
/// - **Version 33** appends the editor messages: the
///   host's `OpenEditor { parent, scale }`, `CloseEditor` and `SetEditorSize`,
///   and the server's `EditorOpened`, `EditorSized`, `EditorWantsSize` and
///   `EditorClosed`, so a served plugin's editor can live in the server. They
///   follow every earlier variant, which keep their numbers.
/// - **Version 34** splits the audio phase into two connections. No message changed: the host
///   connects a third time after the
///   audio connection, the server sends `Ready` on it, and the audio connection
///   carries `ProcessAudio` and `Reset` alone (the server's render thread
///   serves it) while the control connection carries every other request and
///   the plugin's unsolicited news (its main thread serves it, and an editor's
///   paint may keep it busy). A version-33 server accepts no third connection.
/// - **Version 35** says what a state is for: `SaveState`, a state's
///   load and the `StagedState` a load declares carry a
///   `StateUse` (the project's own, or a duplicate's),
///   which a format that distinguishes them (CLAP's `state-context`) acts on.
/// - **Version 36** appends the server's `ProcessRequested`: the plugin asked
///   to be processed again (CLAP's `request_process`), so a host that parked
///   the node on the plugin's *sleep* wakes it. It follows every earlier
///   variant, which keep their numbers.
/// - **Version 37** appends the host's `SetTrackInfo` (what the host says of
///   the plugin's track, for CLAP's `track-info`) and the server's
///   `TransportRequested` (the plugin's request to the transport, CLAP's
///   `transport-control`). Both follow every earlier variant, which keep their
///   numbers.
/// - **Version 38** writes an editor size as a `dpi::Size`, which says whether it
///   is in physical pixels or logical points: `SetEditorSize`, `EditorOpened`,
///   `EditorSized` and `EditorWantsSize` carried a pair of integers with no unit.
/// - **Version 39** removed three messages nothing sent (the server's
///   instance-destroyed notice, the host's single-parameter declaration query
///   and its reply); the variants after them renumber.
/// - **Version 40** added `format` to `LoadPlugin`, after `path`: the server
///   opens the file as that format, where it chose the format by the path's
///   extension and so could not open a link whose target has another one.
/// - **Version 41** adds to a plugin's wiring whether it has a bypass of its own
///   and the controller table it declares, to a parameter its role (now a
///   `ControlRole`) and its overview mark.
/// - **Version 42** writes a descriptor's `version` as a `PluginVersion` (absent, text,
///   integer or dotted) where it was a string.
/// - **Version 43** gives `PluginIoChanged` the buses the plugin has now, when they
///   differ from the ones the host was last told: the host's picture of a
///   served plugin's ports followed none of its changes.
/// - **Version 44** adds `refused` to `AudioProcessed`: the server did not
///   publish the block's audio (its input slot was recycled before it ran), so
///   the host counts the chunk refused rather than leaving it unexplained.
/// - **Version 45** writes a block's `TransportInfo` on its units (a `SampleRate`,
///   a `Bpm`, a `Beat` for the playhead and the bar, a `LoopRange`), one position
///   for each of the quarter and beat pairs, and the cycle as an optional region
///   in place of the cycle-active flag.
/// - **Version 46** replaces `LoadPlugin`'s `load` (a string choice of plugin)
///   with `plugin`, an optional `PluginId`, and `PluginLoaded` names the plugin
///   it opened (`id`); a descriptor carries no id.
/// - **Version 47**: `PluginDescriptor` carries `widths`.
/// - **Version 48** replaces the host's `SetParameter` (a write on the control
///   connection) with `ApplyParameters` (the parameters no block has carried,
///   on the audio connection in order with the blocks, answered by
///   `ParametersApplied`): a write now rides the next block's parameter
///   changes, and is sent as its own message only ahead of a request that
///   observes the plugin (its state, a parameter's value). The server applies it
///   through the format's own route for parameters outside a block.
/// - **Version 49** adds `plain_map` to a parameter (`PlainMap`: the line between
///   the declared ends, or the line on its integers; absent when the plugin
///   defines the map), a field of every entry of `ParameterList`, so a
///   version-48 server's list reads differently from the first parameter's last
///   byte.
/// - **Version 50** splits the host's and the server's messages by connection
///   (`AudioRequest`/`AudioReply` on the audio connection, `ControlRequest`/
///   `ControlReply`/`News` on the control one), so a message of the other
///   connection has no variant to be written as.
/// - **Version 51** gives each connection's wire types their own frame cap
///   (`Framed::CAP`), and every chunk of a state, in either direction, carries
///   the `total` of the whole state, so the receiver sizes its buffer once and
///   refuses a total over the limit before it allocates.
/// - **Version 52** numbers the requests: a control request travels as a
///   `Request` with its `ReqId`, an `ApplyParameters` carries one, and an answer
///   echoes it (`ControlFrame::Reply`, `ParametersApplied`), so the host matches an
///   answer to its request by the id and not by the order of the stream. The audio
///   connection's `Error` is gone (nothing sent it), and the restore notice becomes
///   `ControlFrame::Working`, which the server also sends while it opens the
///   plugin, once a second, for the host's wait to restart on; the control
///   connection's `Ready` is a frame of its own and not a reply.
/// - **Version 53** drops `LoadPlugin`'s `shm_name`, which the server never read:
///   the slab is named only in `SetupSharedMemory`, as an opaque `SlabHandle`.
/// - **Version 54** makes the slab anonymous on unix: `SetupSharedMemory` carries a
///   `SlabWire` (nothing, on unix) and the slab itself follows the frame as one
///   byte carrying its descriptor (as ancillary data), a sealed `memfd` on Linux and an
///   unlinked POSIX shared memory object on macOS. The server refuses a setup whose
///   descriptor does not arrive, and on Linux one that is not a sealed memfd of the
///   layout's exact length. The slab's rings also start on a cache line of their
///   own, so the layout's offsets differ from version 53's.
/// - **Version 55** sends a plugin state as one memory object beside its frame:
///   `LoadState`, `StageState` and `StateReady` declare its length (`len`) and
///   carry its `BlobWire` (nothing, on unix), and the object follows the frame as
///   a descriptor, as the slab's does, in either direction. They replace the
///   messages that carried a state in chunks, so no answer has more than one
///   frame. The receiver refuses an object of another length than
///   declared, a length over `MAX_STATE_BYTES`, and on Linux one that is not a
///   sealed memfd.
pub const PROTOCOL_VERSION: u32 = 55;

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
    AudioReply, AudioRequest, ControlFrame, ControlReply, ControlRequest, News, ReqId, Request,
    StagedState, Working,
};
pub use frame::Framed;
pub use process::ProcessAudioData;
pub use shm::SlabLayout;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tutti_buffers::FloatWidth;

    use tutti_plugin_types::{
        ChordValue, MidiEventVec, NoteExpressionIntValue, NoteExpressionTextValue, ParameterInfo,
        PluginFormat, PluginId, PluginTail, Preset, PresetId, Samples, ScaleValue, TransportInfo,
        Uid, WireMidi,
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
                    Preset::new(PresetId::Number(0), "Init"),
                    Preset::in_bank(
                        PresetId::Program {
                            list_id: 1,
                            index: 2,
                        },
                        "Warm",
                        "Bank 1",
                    ),
                ],
            },
            ControlReply::PresetLoaded { ok: true },
            ControlReply::PresetLoaded { ok: false },
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
                error: None,
            },
            ControlReply::SampleRateSet {
                rate: SampleRate::SR_96K,
                error: Some("refused".to_string()),
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

    /// **Each enum's tags are its declaration order, pinned here as literals.**
    ///
    /// postcard encodes an enum as a leading varint discriminant over
    /// declaration order, so inserting a variant mid-enum silently renumbers
    /// every later one, and a symmetric round trip cannot catch it because both
    /// ends move together. The two ends are not one build: a host and its
    /// server negotiate `PROTOCOL_VERSION` and then trust each other's bytes. A
    /// change of order fails here until the version moves and the pins with it.
    #[test]
    fn the_wire_tags_are_the_declaration_order() {
        use tutti_plugin_types::{AutomationMode, RenderMode, StateUse};
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
            })
            .unwrap()),
            0
        );
        assert_eq!(
            tag(
                postcard::to_allocvec(&AudioReply::ParametersApplied { id: ReqId::new(1) })
                    .unwrap()
            ),
            2
        );

        let control = |m: ControlRequest| tag(postcard::to_allocvec(&m).unwrap());
        assert_eq!(control(ControlRequest::UnloadPlugin), 1);
        assert_eq!(
            control(ControlRequest::SetAutomationState {
                mode: AutomationMode::default()
            }),
            2
        );
        assert_eq!(control(ControlRequest::GetParameterList), 4);
        assert_eq!(
            control(ControlRequest::SaveState {
                state_use: StateUse::Project
            }),
            6
        );
        assert_eq!(
            control(ControlRequest::SetRenderMode {
                mode: RenderMode::Realtime
            }),
            10
        );
        assert_eq!(control(ControlRequest::GetPresetList), 11);
        assert_eq!(control(ControlRequest::GetCurrentPreset), 13);
        assert_eq!(control(ControlRequest::CloseEditor), 20);

        let reply = |m: ControlReply| tag(postcard::to_allocvec(&m).unwrap());
        assert_eq!(reply(ControlReply::ParameterValue { value: None }), 1);
        assert_eq!(reply(ControlReply::SharedMemoryReady), 7);
        assert_eq!(reply(ControlReply::Shutdown), 9);
        assert_eq!(reply(ControlReply::ParameterNormalized { value: None }), 15);

        let news = |m: News| tag(postcard::to_allocvec(&m).unwrap());
        assert_eq!(news(News::PluginParamValuesChanged), 3);
        assert_eq!(news(News::PluginReloaded), 6);
        assert_eq!(news(News::EditorClosed), 8);
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
            frame(ControlFrame::Ready {
                protocol_version: 1
            }),
            3
        );
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
            } => {
                assert!(refused);
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
    /// `LoadedPlugin.tail` wire test one level up, on the *update* path —
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
        const BYTES: [u8; 27] = [
            0, // variant
            42, // latency_us
            7, // seq
            2, // two events
            5, 0xC0, 0xF8, 0xC0, 0x84, 0x02, 0, 0, 0, // offset 5, 0x2090_3C40, 0, 0, 0
            0x84, 0x86, 0x88, 0x08, 1, 2, 3, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, // 0x0102_0304; 1, 2, 3, u32::MAX
            2, // status: sleep
            1, // refused
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
            .with_overview(true);
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
