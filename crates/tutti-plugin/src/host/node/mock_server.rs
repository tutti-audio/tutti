//! A mock `plugin-server` for `tutti-plugin`'s node tests: a doubling plugin
//! that echoes each chunk's MIDI as its MIDI-out, records the audio-path frames
//! it is sent as the raw bytes of the wire, and the helpers that build the
//! generic node over [`Ipc`] against it.

use std::sync::Arc;
use std::time::Duration;
use tutti_rt::Mutex;

use tutti_buffers::FloatWidth;
use tutti_graph::Offset;
use tutti_graph::contract::Direct;
use tutti_plugin_host::client_over;
use tutti_plugin_ipc::{
    AudioReply, AudioRequest, AudioSlab, ControlFrame, ControlReply, ControlRequest, Framed, Ipc,
    IpcParts, PluginBridge, ProcessAudioData, ProcessGuard, RING_SLOTS, Request, SlabLayout,
    SlabWriter,
};
use tutti_plugin_types::{
    ChannelLayout, EditorPresence, Features, MaxBlock, ParameterChanges, ParameterInfo, PluginBus,
    PluginClass, PluginDescriptor, PluginTail, PluginWiring, Prepare, PresetId,
};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_timeline::TempoReader;
use tutti_units::ParamId;
use tutti_units::{Beat, Bpm, SampleRate, Samples};

pub(super) const SR: SampleRate = SampleRate::SR_48K;
/// The slab's chunk ceiling, and the block the graph is prepared for: the
/// pipeline's chunk is this, as no quantum is given.
pub(super) const CHUNK: usize = 128;
/// The tempo map's step, in beats, and the frame it lands on at 120 BPM.
pub(super) const STEP_BEAT: f64 = 4.01;
pub(super) const STEP_FRAME: u64 = 240;
/// Beats per frame at 120 BPM and at 90 BPM, at 48 kHz.
pub(super) const BEATS_PER_FRAME_120: f64 = 2.0 / 48_000.0;
pub(super) const BEATS_PER_FRAME_90: f64 = 1.5 / 48_000.0;

/// The playhead at stream frame `frame`, rolling from beat 4: at the map's
/// tempo, so a block starting where the last ended continues the stretch (a
/// playhead that did not is a seek, which the out-of-process node reports at the call
/// and this node, reading each piece apart, does not).
pub(super) fn beat_at(frame: u64) -> f64 {
    if frame <= STEP_FRAME {
        4.0 + frame as f64 * BEATS_PER_FRAME_120
    } else {
        STEP_BEAT + (frame - STEP_FRAME) as f64 * BEATS_PER_FRAME_90
    }
}
pub(super) const PARAMS: [u32; 2] = [7, 9];
/// The preset the mock plugin reports as loaded, and the state it saves.
pub(super) const PRESET: i32 = 42;
pub(super) const STATE: [u8; 3] = [3, 1, 4];

/// The bytes of every audio-path frame the mock server was sent.
pub(super) type Record = Arc<Mutex<Vec<Said>>>;

/// A request the server was sent that a test reads back, on either connection.
#[allow(clippy::large_enum_variant)] // a test record, built once
pub(super) enum Said {
    Audio(AudioRequest),
    Control(ControlRequest),
}

/// A bridge, its mock server, and what the server was sent.
pub(super) struct Rig {
    pub(super) bridge: Arc<PluginBridge>,
    pub(super) slab: SlabWriter,
    pub(super) replies: tutti_plugin_ipc::Replies,
    pub(super) record: Record,
    pub(super) descriptor: PluginDescriptor,
    pub(super) loaded: PluginWiring,
    pub(super) threads: Threads,
}

/// The bridge thread and the server thread: the rig lives while they do.
pub(super) struct Threads {
    _bridge: tutti_plugin_ipc::IpcThread,
    _server: std::thread::JoinHandle<()>,
}

fn layout() -> SlabLayout {
    SlabLayout {
        samples_per_channel: CHUNK,
        format: FloatWidth::F32,
        slots: RING_SLOTS as u32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    }
}

fn no_editor(
    _: tutti_plugin_types::MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    plugin: &tutti_plugin_types::PluginRef,
    _: tutti_plugin_types::PluginFormat,
) -> tutti_plugin_types::IpcResult<Box<dyn tutti_plugin_types::Editor>> {
    Err(tutti_plugin_types::IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "no editor in these tests".into(),
    })
}

/// A frame off the socket, whole, with its decoding. `None` at EOF.
fn read_frame<T: Framed>(stream: &interprocess::local_socket::Stream) -> Option<T> {
    use std::io::Read;
    let mut stream = stream;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).ok()?;
    let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut bytes).ok()?;
    postcard::from_bytes(&bytes).ok()
}

fn send<T: Framed>(stream: &interprocess::local_socket::Stream, message: &T) -> bool {
    let mut stream = stream;
    tutti_plugin_ipc::write_frame(&mut stream, message).is_ok()
}

/// The audio connection: a block is answered from the slab and logged, as is a
/// reset.
fn serve_audio(
    stream: &interprocess::local_socket::Stream,
    mut slab: SlabWriter,
    log: &Record,
    gate: Option<ReplyGate>,
) {
    let mut scratch = vec![0.0f32; CHUNK];
    while let Some(message) = read_frame::<AudioRequest>(stream) {
        let reply = match message {
            AudioRequest::ProcessAudio(data) => {
                log.lock()
                    .push(Said::Audio(AudioRequest::ProcessAudio(data.clone())));
                let reply = answer(&mut slab, &mut scratch, data);
                if let Some(gate) = &gate {
                    let _ = gate.published.send(());
                    let _ = gate.release.recv_timeout(Duration::from_secs(60));
                }
                Some(reply)
            }
            AudioRequest::Reset => {
                log.lock().push(Said::Audio(AudioRequest::Reset));
                None
            }
            AudioRequest::ApplyParameters { id, .. } => Some(AudioReply::ParametersApplied {
                id,
                outcome: tutti_plugin_ipc::ApplyOutcome::Applied,
            }),
        };
        if let Some(reply) = reply
            && !send(stream, &reply)
        {
            break;
        }
    }
}

/// The control connection: the queries a node makes at load and a rate change.
fn serve_control(stream: &interprocess::local_socket::Stream, log: &Record, lists: bool) {
    while let Some(Request { id, body }) = read_frame::<Request>(stream) {
        let reply = match body {
            // A server that cannot list answers with another reply, which is
            // no list to the bridge.
            ControlRequest::GetParameterList if !lists => Some(ControlReply::PresetList {
                presets: Vec::new(),
            }),
            ControlRequest::GetParameterList => Some(ControlReply::ParameterList {
                parameters: PARAMS
                    .iter()
                    .map(|&id| ParameterInfo::new(ParamId::new(id), "p"))
                    .collect(),
            }),
            ControlRequest::GetPresetList => Some(ControlReply::PresetList {
                presets: Vec::new(),
            }),
            ControlRequest::GetCurrentPreset => Some(ControlReply::CurrentPreset {
                id: Some(PresetId::Number(PRESET)),
            }),
            ControlRequest::SaveState { .. } => {
                if tutti_plugin_ipc::send_state_reply(stream, id, &STATE).is_err() {
                    break;
                }
                None
            }
            ControlRequest::SetSampleRate { rate } => {
                log.lock()
                    .push(Said::Control(ControlRequest::SetSampleRate { rate }));
                Some(ControlReply::SampleRateSet {
                    rate,
                    refusal: None,
                })
            }
            ControlRequest::SetRenderMode { mode } => Some(ControlReply::RenderModeSet {
                mode,
                outcome: tutti_plugin_ipc::ModeOutcome::Took,
            }),
            _ => None,
        };
        if let Some(reply) = reply
            && !send(stream, &ControlFrame::Reply { id, reply })
        {
            break;
        }
    }
}

pub(super) fn rig() -> Rig {
    rig_gated(None)
}

/// [`rig`] whose server cannot list the plugin's parameters.
pub(super) fn rig_unlisted() -> Rig {
    build(None, false)
}

/// What holds a mock server's replies: the server signals `published` once a
/// block's audio is in the slab, then sends the reply only when `release`
/// gets a message. The test owns the other ends.
pub(super) struct ReplyGate {
    pub(super) published: std::sync::mpsc::Sender<()>,
    pub(super) release: std::sync::mpsc::Receiver<()>,
}

/// [`rig`] whose server holds each block's reply behind `gate`, if given.
pub(super) fn rig_gated(gate: Option<ReplyGate>) -> Rig {
    build(gate, true)
}

fn build(gate: Option<ReplyGate>, lists: bool) -> Rig {
    let ((audio_stream, audio), (control_stream, control)) = (
        tutti_plugin_ipc::broker_pair(),
        tutti_plugin_ipc::broker_pair(),
    );
    let (slab, handle) = AudioSlab::create(layout()).unwrap();
    let server_slab = AudioSlab::open(&handle, layout()).unwrap();
    let (bridge, thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        audio_stream,
        control_stream,
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(tutti_plugin_ipc::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        SR,
        Arc::new(tutti_plugin_ipc::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();
    let record: Record = Arc::default();
    let log = Arc::clone(&record);
    std::thread::Builder::new()
        .name("mock-differential-control".into())
        .spawn({
            let log = Arc::clone(&record);
            move || serve_control(&control, &log, lists)
        })
        .unwrap();
    let server = std::thread::Builder::new()
        .name("mock-differential-server".into())
        .spawn(move || {
            serve_audio(&audio, server_slab, &log, gate);
        })
        .unwrap();
    Rig {
        bridge,
        slab,
        replies,
        record,
        descriptor: PluginDescriptor {
            name: "Test Plugin".into(),
            vendor: String::new(),
            version: tutti_plugin_types::PluginVersion::Absent,
            class: PluginClass::Unknown,
            editor: EditorPresence::Absent,
            measured: None,
            widths: None,
        },
        loaded: PluginWiring {
            inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            latency_samples: Samples::ZERO,
            tail: PluginTail::Unknown,
            features: Features::TRANSPORT
                | Features::RENDER_MODE
                | Features::MIDI_OUT
                | Features::PARAM_AUTOMATION
                | Features::SEQUENCER_CONTEXT,
            ..PluginWiring::default()
        },
        threads: Threads {
            _bridge: thread,
            _server: server,
        },
    }
}

/// The plugin: every channel doubled, the chunk's MIDI echoed as its MIDI-out.
fn answer(slab: &mut SlabWriter, scratch: &mut [f32], data: ProcessAudioData) -> AudioReply {
    let n = data.num_samples;
    if slab.has_input(data.seq) {
        for ch in 0..2 {
            let got = slab
                .read_input_into(data.seq, ch, &mut scratch[..n])
                .unwrap_or(0);
            for s in &mut scratch[..got] {
                *s *= 2.0;
            }
            slab.write(data.seq, ch, &scratch[..got]).unwrap();
        }
        slab.publish(data.seq);
    }
    AudioReply::AudioProcessed {
        latency_us: 0,
        seq: data.seq,
        midi_out: data.midi_events,
        status: Default::default(),
        refused: false,
    }
}

/// A map at 120 BPM that steps to 90 at `beat`.
pub(super) fn tempo(beat: f64) -> TempoReader {
    let point = |beat: f64, bpm: f64| TempoPoint {
        beat: Beat::new(beat),
        tempo: Bpm::of(bpm),
        transition: Transition::Step,
    };
    TempoReader::fixed(Arc::new(
        TempoMap::new(vec![point(0.0, 120.0), point(beat, 90.0)]).expect("a map"),
    ))
}

/// What the new node reads: the generic node over [`Ipc`], a fork prepared
/// offline.
pub(super) fn new_node(
    rig: Rig,
    tempo: TempoReader,
) -> (Direct<tutti_plugin_host::PluginNode<Ipc>>, Record, Threads) {
    let fork = fork_node(rig, tempo, tutti_graph::RenderMode::Offline);
    (fork.unit, fork.record, fork.threads)
}

/// A served fork node, as `ServedFork` builds one: watched, handed its wait's
/// switch, and prepared in the graph mode `mode`.
pub(super) struct ForkNode {
    pub(super) unit: Direct<tutti_plugin_host::PluginNode<Ipc>>,
    pub(super) record: Record,
    pub(super) threads: Threads,
    /// What the pipeline waits on: armed while the node's graph is offline.
    pub(super) wait: Arc<tutti_plugin_ipc::OfflineWait>,
    /// The surface the app tells the plugin its render mode through.
    pub(super) any_thread: Arc<dyn tutti_plugin_types::AnyThread>,
}

pub(super) fn fork_node(rig: Rig, tempo: TempoReader, mode: tutti_graph::RenderMode) -> ForkNode {
    let (mut ipc, record, threads) = served(rig);
    let (wait, switch) = ipc.watch();
    let prepare = Prepare::new(SR, MaxBlock::new(Samples::new(CHUNK)).expect("a block"));
    let descriptor = ipc.descriptor().clone();
    let wiring = ipc.loaded().clone();
    let any_thread: Arc<dyn tutti_plugin_types::AnyThread> = Arc::new(ipc.controller());
    let (processor, controller) = ipc
        .activate_anywhere(prepare)
        .unwrap_or_else(|_| panic!("the served plugin activates"));
    let mut client = client_over::<Ipc, f32>(
        processor,
        controller,
        prepare,
        descriptor,
        &wiring,
        PARAMS.iter().map(|&id| ParamId::new(id)),
        ParameterChanges::SPARE_QUEUES,
        None,
    )
    .expect("the wiring is within a block's width");
    client.go_offline();
    client.adopt_wait_switch(switch);
    // The graph's prepare is what makes the served pipeline wait for a chunk.
    let graph = tutti_graph::Prepare::new(SR, MaxBlock::new(Samples::new(CHUNK)).expect("a block"))
        .with_render_mode(mode);
    ForkNode {
        unit: Direct::with_prepare(client.bind(tempo), &graph),
        record,
        threads,
        wait,
        any_thread,
    }
}

/// The served plugin over the mock server's bridge, not yet activated.
pub(super) fn served(rig: Rig) -> (Ipc, Record, Threads) {
    let guard = Arc::new(ProcessGuard::for_test());
    let record = Arc::clone(&rig.record);
    let threads = rig.threads;
    let ipc = Ipc::new(IpcParts {
        bridge: rig.bridge,
        guard: Arc::clone(&guard),
        descriptor: rig.descriptor,
        loaded: rig.loaded,
        format: FloatWidth::F32,
        ceiling: CHUNK,
        slab: rig.slab,
        replies: rig.replies,
        rate: SR,
        parameters: PARAMS.len(),
        timeout: Duration::from_secs(5),
        origin: None,
    });
    (ipc, record, threads)
}

pub(super) fn at(frame: usize, len: usize) -> Offset {
    Offset::of(Samples::new(frame), Samples::new(len))
}
