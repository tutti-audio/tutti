//! [`Ipc`] over a mock `plugin-server`: what its three halves answer, without a
//! plugin format or a real server. The differential test against the served
//! node, and the real-server suites, are `tutti-plugin`'s.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_fixtures::{machine_lock, main_thread};
use tutti_plugin_types::{
    ChannelLayout, Control, Controller, Loaded, LoadedPlugin, MaxBlock, ParamAccess, ParamId,
    ParameterInfo, PluginBus, PluginDescriptor, Prepare, Processor, Reopen, Request, SampleFormat,
    SampleRate, Samples,
};

use super::DeclaredCache;

use crate::{
    AudioSlab, BridgeConfig, BridgeEvent, BridgeMessage, HostMessage, Ipc, IpcController, IpcParts,
    IpcProcessor, PROTOCOL_VERSION, PluginBridge, ProcessGuard, RING_SLOTS, SlabLayout,
    unique_socket_path,
};

const CEILING: usize = 64;
const RATE: SampleRate = SampleRate::SR_48K;
/// The size the mock server's editor opens at.
const MOCK_EDITOR: dpi::Size = dpi::Size::Physical(dpi::PhysicalSize::new(640, 480));

/// What the mock server was sent, in order.
type Received = Arc<Mutex<Vec<HostMessage>>>;

/// A served plugin over a mock server, which records what it is sent, answers
/// the asks the tests make, and sends what a test injects.
struct Mock {
    received: Received,
    /// The parameters the server declares when asked (`GetParameterList`).
    parameters: Arc<Mutex<Vec<ParameterInfo>>>,
    stream: Arc<interprocess::local_socket::Stream>,
    _bridge: crate::BridgeThread,
}

impl Mock {
    /// The server says something unprompted, as a plugin does.
    fn say(&self, message: &BridgeMessage) {
        let mut stream = &*self.stream;
        crate::protocol::frame::write_frame(&mut stream, message).unwrap();
    }

    /// Waits for `want` messages of the server's record to satisfy `found`.
    fn wait_for(&self, found: impl Fn(&[HostMessage]) -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if found(&self.received.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

fn no_editor(
    _: tutti_plugin_types::MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    path: &std::path::Path,
    _: tutti_plugin_types::PluginFormat,
    _: &tutti_plugin_types::Load,
) -> tutti_plugin_types::BridgeResult<Box<dyn tutti_plugin_types::Editor>> {
    Err(tutti_plugin_types::BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "no editor in these tests".into(),
    })
}

fn mock() -> (Ipc, Mock) {
    mock_loading(
        tutti_plugin_types::Origin::new("test.vst3", tutti_plugin_types::Load::new()),
        tutti_plugin_types::PluginFormat::Vst3,
        crate::EditorLoader::new(no_editor, tutti_fixtures::detached_work()),
    )
}

/// [`mock`] whose bridge was made for `origin` and loads its editor with
/// `load_editor`.
fn mock_loading(
    origin: tutti_plugin_types::Origin,
    format: tutti_plugin_types::PluginFormat,
    load_editor: crate::EditorLoader,
) -> (Ipc, Mock) {
    mock_with(origin, format, Some(load_editor))
}

/// [`mock`] whose bridge has no editor loader: its editor lives in the server.
fn mock_remote() -> (Ipc, Mock) {
    mock_with(
        tutti_plugin_types::Origin::new("test.clap", tutti_plugin_types::Load::new()),
        tutti_plugin_types::PluginFormat::Clap,
        None,
    )
}

fn mock_with(
    origin: tutti_plugin_types::Origin,
    format: tutti_plugin_types::PluginFormat,
    load_editor: Option<crate::EditorLoader>,
) -> (Ipc, Mock) {
    use interprocess::local_socket::{ListenerOptions, traits::Listener as _};
    use std::io::Read;
    let layout = SlabLayout {
        samples_per_channel: CEILING,
        format: SampleFormat::Float32,
        slots: RING_SLOTS as u32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    };
    let shm = tutti_fixtures::unique_name("ipc");
    let path = unique_socket_path().expect("the OS has a random source");
    let _ = std::fs::remove_file(&path);
    let listener = ListenerOptions::new()
        .name(crate::socket_name(&path).unwrap())
        .create_sync()
        .unwrap();
    let slab = AudioSlab::create(shm, layout).unwrap();
    let (bridge, thread) = PluginBridge::new(
        path.clone(),
        Arc::clone(slab.slab()),
        origin,
        format,
        load_editor,
        RATE,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();
    // The audio connection first, then the control one, as the server accepts
    // them. Blocks and resets arrive on the first and are only logged.
    let audio = Arc::new(listener.accept().unwrap());
    let reply = |stream: &interprocess::local_socket::Stream, message: &BridgeMessage| {
        let mut stream = stream;
        crate::protocol::frame::write_frame(&mut stream, message).unwrap();
    };
    // The host connects the control stream only once it has the audio one's
    // `Ready`.
    reply(
        &audio,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let stream = Arc::new(listener.accept().unwrap());
    reply(
        &stream,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let received: Received = Arc::default();
    let parameters: Arc<Mutex<Vec<ParameterInfo>>> =
        Arc::new(Mutex::new(vec![ParameterInfo::new(
            ParamId::new(3),
            "gain",
        )]));
    std::thread::spawn({
        let (log, audio) = (Arc::clone(&received), Arc::clone(&audio));
        move || {
            loop {
                let mut len = [0u8; 4];
                let mut stream = &*audio;
                if stream.read_exact(&mut len).is_err() {
                    break;
                }
                let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
                if stream.read_exact(&mut bytes).is_err() {
                    break;
                }
                let Ok(message) = postcard::from_bytes::<HostMessage>(&bytes) else {
                    break;
                };
                log.lock().unwrap().push(message);
            }
        }
    });
    let (log, served) = (Arc::clone(&received), Arc::clone(&stream));
    let answers = Arc::clone(&parameters);
    std::thread::spawn(move || {
        loop {
            let mut len = [0u8; 4];
            let mut stream = &*served;
            if stream.read_exact(&mut len).is_err() {
                break;
            }
            let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
            if stream.read_exact(&mut bytes).is_err() {
                break;
            }
            let Ok(message) = postcard::from_bytes::<HostMessage>(&bytes) else {
                break;
            };
            // Recorded before it is answered: a count read after the reply sees it.
            log.lock().unwrap().push(message.clone());
            match &message {
                HostMessage::SetSampleRate { rate } => reply(
                    &served,
                    &BridgeMessage::SampleRateSet {
                        rate: *rate,
                        error: None,
                    },
                ),
                // The server's editor: a parent of 0 is a window it cannot embed
                // into; any other opens at the size the test pins.
                HostMessage::OpenEditor { parent, .. } => reply(
                    &served,
                    &BridgeMessage::EditorOpened {
                        result: match parent {
                            Some(0) => Err("the mock plugin has no editor".to_string()),
                            Some(_) => Ok(Some(MOCK_EDITOR)),
                            None => Ok(None),
                        },
                    },
                ),
                HostMessage::SetEditorSize { size } => {
                    reply(&served, &BridgeMessage::EditorSized { result: Ok(*size) })
                }
                HostMessage::GetParameterList => reply(
                    &served,
                    &BridgeMessage::ParameterList {
                        parameters: answers.lock().unwrap().clone(),
                    },
                ),
                _ => {}
            }
        }
        let _ = std::fs::remove_file(&path);
    });
    std::thread::sleep(Duration::from_millis(50));
    let ipc = Ipc::new(IpcParts {
        bridge,
        guard: Arc::new(ProcessGuard::for_test(BridgeConfig::at(
            crate::unique_socket_path().expect("the OS has a random source"),
        ))),
        descriptor: PluginDescriptor::default(),
        loaded: LoadedPlugin {
            inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            latency_samples: Samples(5),
            ..LoadedPlugin::default()
        },
        format: SampleFormat::Float32,
        ceiling: CEILING,
        slab,
        rate: RATE,
        parameters: 1,
        timeout: Duration::from_secs(1),
        origin: None,
    });
    (
        ipc,
        Mock {
            received,
            parameters,
            stream,
            _bridge: thread,
        },
    )
}

fn prepare(block: usize) -> Prepare {
    Prepare::new(RATE, MaxBlock::of(Samples(block)))
}

fn activate(ipc: Ipc, block: usize) -> (IpcProcessor, IpcController) {
    match ipc.activate(main_thread(), prepare(block)) {
        Ok(halves) => halves,
        Err(refused) => panic!("{}", refused.reason),
    }
}

/// **`IpcProcessor::chunk()` is its configured chunk**: the block the plugin
/// was activated for, never past the slab's ceiling. It is what the graph
/// node cuts a block by, so a Processor that answered the trait's default
/// (`None`) would be handed whole blocks that span chunks.
#[test]
fn the_processor_chunk_is_its_configured_chunk() {
    let _machine = machine_lock();
    let (ipc, _server) = mock();
    let (processor, _controller) = activate(ipc, 48);
    assert_eq!(processor.chunk(), NonZeroUsize::new(48));
    let (ipc, _server) = mock();
    let (past, _controller) = activate(ipc, 1000);
    assert_eq!(
        past.chunk(),
        NonZeroUsize::new(CEILING),
        "the slab's ceiling"
    );
}

/// **A Processor follows the graph without a restart**: `retune` settles the
/// chunk from the device quantum when there is one (the block otherwise), the
/// latency it adds is that chunk, and the server is told the rate by a queued
/// command. Taken off the main thread's proof, which is the point.
#[test]
fn a_processor_follows_the_graphs_rate_block_and_quantum() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let received = Arc::clone(&server.received);
    let (mut processor, _controller) = activate(ipc, 48);
    assert!(processor.retune(SampleRate::SR_96K, Samples(60), Some(Samples(40))));
    assert_eq!(processor.chunk(), NonZeroUsize::new(40), "the quantum");
    assert_eq!(processor.latency(), Samples(40), "one chunk of latency");
    assert!(processor.retune(SampleRate::SR_96K, Samples(30), None));
    assert_eq!(processor.chunk(), NonZeroUsize::new(30), "the block");
    let start = Instant::now();
    while !received
        .lock()
        .unwrap()
        .iter()
        .any(|m| matches!(m, HostMessage::SetSampleRate { rate } if *rate == SampleRate::SR_96K))
    {
        assert!(start.elapsed() < Duration::from_secs(5), "the rate is sent");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **Only the two halves of one activation rejoin.** A pair from two
/// activations is refused with both halves back (`Stuck`); the pair that was
/// split rejoins, and can be activated again.
#[test]
fn only_the_halves_of_one_activation_rejoin() {
    let _machine = machine_lock();
    let main = main_thread();
    let ((first, _a), (second, _b)) = (mock(), mock());
    let (first_p, first_c) = activate(first, 32);
    let (second_p, second_c) = activate(second, 32);
    let Err(stuck) = Ipc::deactivate(main, first_p, second_c) else {
        panic!("a pair from two activations must not rejoin");
    };
    let (first_p, second_c) = (stuck.processor, stuck.controller);
    let again = Ipc::deactivate(main, first_p, first_c).expect("one activation rejoins");
    assert!(again.activate(main, prepare(32)).is_ok());
    drop((second_p, second_c));
}

/// **The controller reports what the plugin re-reports, and the host hears
/// it.** The server says the latency changed: the controller's latency is the
/// plugin's new figure (the pipeline's chunk is the Processor's), the
/// lifecycle is asked to re-read it (a `Request::LatencyChanged`, once), and a
/// host that forwarded the bridge's events gets the event too. Parameters and
/// the descriptor are the bridge's and the loaded plugin's, with no main
/// thread's work in them.
#[test]
fn the_controller_reports_what_the_plugin_re_reports() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    ipc.forward_events(Some(Arc::new(move |event| {
        sink.lock().unwrap().push(format!("{event:?}"));
    })));
    let say = |latency| server.say(&BridgeMessage::LatencyChanged { samples: latency });
    let main = main_thread();
    let (_processor, mut controller) = activate(ipc, 32);
    assert_eq!(controller.latency(main), Samples(5));
    say(Samples(77));
    // An idle bridge reads nothing: the next command's reply reads it.
    assert_eq!(controller.parameters(main).len(), 1);
    // The bridge's thread stores the latency, asks for the re-read, then
    // forwards the event, in that order: once the host has heard it, all
    // three are done.
    let start = Instant::now();
    while !heard
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.contains("LatencyChanged"))
    {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the host hears it"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(controller.latency(main), Samples(77));
    let mut asked = Vec::new();
    controller.take_requests(main, &mut asked);
    assert_eq!(asked, [Request::LatencyChanged]);
    controller.take_requests(main, &mut asked);
    assert_eq!(asked.len(), 1, "once");
    assert!(server.wait_for(|got| {
        got.iter()
            .any(|m| matches!(m, HostMessage::GetParameterList))
    }));
}

/// **The buses a plugin announces are the ones the host sees, and not before.**
/// The plugin has a stereo bus in and out at load; it announces a stereo main bus
/// and a mono aux bus on the way in. The controller's layout, and the `wiring`
/// its `Control` answers, follow the announcement; the slab's layout (what
/// [`Ipc::loaded`] says, what the pipeline was built for) stays the one at load. A
/// `PluginIoChanged` with no layout changes nothing.
#[test]
fn the_buses_a_plugin_announces_are_the_buses_the_host_sees() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    ipc.forward_events(Some(Arc::new(move |event| {
        sink.lock().unwrap().push(format!("{event:?}"));
    })));
    let main = main_thread();
    let (_processor, controller) = activate(ipc, 32);
    let at_load = controller.layout();
    assert_eq!(at_load.input_widths(), vec![ChannelLayout::STEREO]);

    server.say(&BridgeMessage::PluginIoChanged { layout: None });
    let announced = tutti_plugin_types::Layout {
        inputs: vec![
            PluginBus::main(ChannelLayout::STEREO),
            PluginBus::new(ChannelLayout::MONO, tutti_plugin_types::BusRole::Aux),
        ],
        outputs: at_load.outputs.clone(),
    };
    server.say(&BridgeMessage::PluginIoChanged {
        layout: Some(announced.clone()),
    });
    // An idle bridge reads nothing: the next command's reply reads it.
    assert_eq!(controller.parameters(main).len(), 1);
    let start = Instant::now();
    while !heard.lock().unwrap().iter().any(|e| e.contains("IoLayout")) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the host hears it"
        );
        std::thread::sleep(Duration::from_millis(5));
        let _ = controller.parameters(main);
    }
    assert_eq!(controller.layout(), announced);
    let wiring = controller.wiring(main);
    assert_eq!(wiring.inputs, announced.inputs, "the controller's wiring");
    assert_eq!(wiring.outputs, announced.outputs);
}

/// **A served plugin with no origin cannot be reopened, and says so**, through
/// both the token-free reopen and the trait's.
#[test]
fn a_plugin_with_no_origin_is_not_reopened() {
    let _machine = machine_lock();
    let (ipc, _server) = mock();
    let err = ipc.reopen_restoring(None).expect_err("no origin");
    assert!(err.to_string().contains("no origin"), "{err}");
    assert!(ipc.reopen(main_thread()).is_err());
}

/// The format and `Load` the last [`recording_editor`] was given.
static EDITOR_LOAD: Mutex<Option<(tutti_plugin_types::PluginFormat, tutti_plugin_types::Load)>> =
    Mutex::new(None);

fn recording_editor(
    _: tutti_plugin_types::MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    path: &std::path::Path,
    format: tutti_plugin_types::PluginFormat,
    load: &tutti_plugin_types::Load,
) -> tutti_plugin_types::BridgeResult<Box<dyn tutti_plugin_types::Editor>> {
    *EDITOR_LOAD.lock().unwrap() = Some((format, load.clone()));
    Err(tutti_plugin_types::BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "records the choice and loads nothing".into(),
    })
}

/// **The editor-only instance loads the plugin the audio instance is**, not
/// the file's first, and as the format the audio instance was launched as,
/// whatever the file's extension: the bridge hands its loader the format and
/// `Load` it was made for.
/// It needs no display (the floating open takes no window), because the loader
/// is asked before any editor exists; a real editor's identity cannot be read
/// without one, so the choice is pinned where it is made.
#[test]
fn the_editor_instance_is_loaded_with_the_choice_the_bridge_was_made_for() {
    let _machine = machine_lock();
    let chosen = tutti_plugin_types::Load::new().plugin("the.second.plugin");
    let (ipc, _peer) = mock_loading(
        tutti_plugin_types::Origin::new("libbundle.so", chosen.clone()),
        tutti_plugin_types::PluginFormat::Clap,
        crate::EditorLoader::new(recording_editor, tutti_fixtures::detached_work()),
    );
    let refused = ipc.bridge().open_floating_editor(main_thread());
    assert!(refused.is_err(), "the recording loader loads no editor");
    assert_eq!(
        *EDITOR_LOAD.lock().unwrap(),
        Some((tutti_plugin_types::PluginFormat::Clap, chosen))
    );
}

/// A window number the mock's X11-style server takes.
#[cfg(all(unix, not(target_os = "macos")))]
fn host_window(raw: u64) -> tutti_plugin_types::WindowHandle {
    tutti_plugin_types::WindowHandle::from_remote(raw).expect("an X11 id embeds across processes")
}

/// **A client with no editor loader opens, resizes and closes its editor over
/// the wire**: the open carries the host window's number and scale and answers
/// with the server's size, the resize is the server's answer, and the close
/// is a message with no editor in this process to load.
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn a_remote_editor_opens_resizes_and_closes_over_the_wire() {
    use tutti_plugin_types::{ContentScale, EditorWindow};
    let _machine = machine_lock();
    let (mut ipc, server) = mock_remote();
    let main = main_thread();
    let scale = ContentScale::new(2.0).expect("a scale");

    let size = ipc
        .open_editor(main, host_window(0x40_0001), scale)
        .expect("the server opens it");
    assert_eq!(size, MOCK_EDITOR);
    let wanted = dpi::Size::Physical(dpi::PhysicalSize::new(801, 601));
    assert_eq!(ipc.set_editor_size(main, wanted).expect("resized"), wanted);
    ipc.close_editor(main);

    assert!(server.wait_for(|got| matches!(
        got,
        [
            HostMessage::OpenEditor { parent: Some(0x40_0001), scale: s },
            HostMessage::SetEditorSize { size },
            HostMessage::CloseEditor,
        ] if *s == scale && *size == wanted
    )));
}

/// **A floating editor is the server's own window**: no parent crosses the
/// wire, and no size comes back.
#[test]
fn a_remote_floating_editor_sends_no_parent() {
    use tutti_plugin_types::EditorWindow;
    let _machine = machine_lock();
    let (mut ipc, server) = mock_remote();
    ipc.open_floating_editor(main_thread()).expect("opens");
    assert!(server.wait_for(|got| matches!(got, [HostMessage::OpenEditor { parent: None, .. }])));
}

/// **The server's refusal is the editor's error, in the server's words.**
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn a_refused_remote_open_carries_the_servers_reason() {
    use tutti_plugin_types::{ContentScale, EditorWindow};
    let _machine = machine_lock();
    let (mut ipc, _server) = mock_remote();
    let refused = ipc
        .open_editor(main_thread(), host_window(0), ContentScale::IDENTITY)
        .expect_err("the mock refuses a window 0");
    assert!(
        refused
            .to_string()
            .contains("the mock plugin has no editor"),
        "{refused}"
    );
}

/// **A size the plugin asks its window to take reaches the host's poll, once,
/// and the server closing its editor is heard**: the bridge reads both off the
/// stream with the next reply, and the editor is closed, so a resize is
/// refused until it is opened again.
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn what_the_servers_editor_says_unprompted_is_heard() {
    use tutti_plugin_types::{ContentScale, EditorWindow};
    let _machine = machine_lock();
    let (mut ipc, server) = mock_remote();
    let main = main_thread();
    ipc.open_editor(main, host_window(0x40_0001), ContentScale::IDENTITY)
        .expect("opens");
    let asked = dpi::Size::Physical(dpi::PhysicalSize::new(1024, 768));
    server.say(&BridgeMessage::EditorWantsSize { size: asked });
    server.say(&BridgeMessage::EditorClosed);
    // An idle bridge reads nothing: the next command's reply reads both.
    assert_eq!(ipc.bridge().parameters().expect("answered").len(), 1);

    assert_eq!(ipc.poll_editor_resize_request(main), Some(asked));
    assert_eq!(ipc.poll_editor_resize_request(main), None, "once");
    let refused = ipc
        .set_editor_size(main, asked)
        .expect_err("the server closed it");
    assert!(refused.to_string().contains("not open"), "{refused}");
    // A close of an editor the server already closed sends nothing: asked
    // after it, in order, a reply the mock answers proves what it was sent.
    ipc.close_editor(main);
    assert_eq!(ipc.bridge().parameters().expect("answered").len(), 1);
    assert!(
        !server
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, HostMessage::CloseEditor)),
        "no close for an editor that is not open"
    );
}

/// **A client with a loader keeps its editor in the host**: nothing about the
/// editor crosses the wire.
#[test]
fn a_client_with_a_loader_sends_no_editor_message() {
    use tutti_plugin_types::EditorWindow;
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    assert!(
        ipc.open_floating_editor(main_thread()).is_err(),
        "no editor in these tests"
    );
    assert_eq!(ipc.bridge().parameters().expect("answered").len(), 1);
    assert!(server.wait_for(|got| {
        got.iter()
            .any(|m| matches!(m, HostMessage::GetParameterList))
    }));
    assert!(
        !server
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, HostMessage::OpenEditor { .. })),
        "the editor is the host's"
    );
}

/// Renders one block of `frames` silent frames through `processor`.
fn render_silence(processor: &mut IpcProcessor, frames: usize) {
    use tutti_plugin_types::{
        AudioBuffer, AudioBufferMut, PluginIo, ProcessContext, ProcessOutput,
    };
    let input = vec![vec![0.0f32; frames]; 2];
    let mut output = vec![vec![0.0f32; frames]; 2];
    let inputs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(Vec::as_mut_slice).collect();
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer {
            inputs: &inputs,
            outputs: &mut outputs,
            num_samples: frames,
            sample_rate: RATE,
        }),
        ctx: &ctx,
        out: &mut out,
    };
    processor.process(&mut io).expect("the block renders");
}

/// The sequences of the `ProcessAudio` messages the mock was sent, in order.
fn process_seqs(server: &Mock, count: usize) -> Vec<u64> {
    let seqs = |log: &[HostMessage]| {
        log.iter()
            .filter_map(|m| match m {
                HostMessage::ProcessAudio(data) => Some(data.seq),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(
        server.wait_for(|log| seqs(log).len() >= count),
        "{count} chunks reach the server"
    );
    seqs(&server.received.lock().unwrap())
}

/// **A reactivation never reuses a chunk sequence.** The slab outlives an
/// activation, and a `ProcessAudio` of the old one can still be in the audio
/// socket when the server publishes: a new activation that restarted its
/// sequence would match that publish as its own chunk and play old audio as
/// new. The sequence of the second activation is past every one of the first.
#[test]
fn a_reactivation_continues_the_chunk_sequence() {
    let _machine = machine_lock();
    let main = main_thread();
    let (ipc, server) = mock();
    let (mut processor, controller) = activate(ipc, 32);
    render_silence(&mut processor, 32);
    render_silence(&mut processor, 32);
    let first = process_seqs(&server, 2);
    let ipc = Ipc::deactivate(main, processor, controller).expect("the halves rejoin");
    let (mut processor, _controller) = activate(ipc, 32);
    render_silence(&mut processor, 32);
    let all = process_seqs(&server, 3);
    let (old, new) = all.split_at(first.len());
    assert!(
        new.iter().all(|n| old.iter().all(|o| n > o)),
        "old {old:?}, new {new:?}"
    );
}

/// How many more blocks the bridge must abandon, after a render of
/// [`CHUNKS_AFTER`] chunks, for it to be off the fork's budget. A bridge held
/// on the budget (1 s here) abandons at most one per second, so within
/// [`WITHIN`] it adds two at most; one off it adds nearly all eight, and a
/// descheduled bridge that skips some (a chunk reached more than `MAX_BEHIND`
/// behind is skipped, not abandoned) still adds more than this.
const OFF_THE_BUDGET: u64 = 4;
const CHUNKS_AFTER: usize = 8;
const WITHIN: Duration = Duration::from_secs(2);

/// Waits for the bridge to have abandoned [`OFF_THE_BUDGET`] blocks past
/// `before`, panicking with what it had when [`WITHIN`] runs out.
fn wait_off_the_budget(bridge: &PluginBridge, before: u64) {
    let start = Instant::now();
    while bridge.abandoned_replies() < before + OFF_THE_BUDGET {
        assert!(
            start.elapsed() < WITHIN,
            "the bridge abandoned {} blocks in {WITHIN:?} after {CHUNKS_AFTER} chunks, wanted \
             {OFF_THE_BUDGET}: it is still holding a block for the fork's budget",
            bridge.abandoned_replies() - before
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits for the bridge to have abandoned one block (the first, at the budget).
fn wait_first_abandoned(bridge: &PluginBridge) {
    let start = Instant::now();
    while bridge.abandoned_replies() < 1 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the bridge never abandoned the first block"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Renders `chunks` more chunks of silence through an offline fork's
/// processor, a bridge-thread turn apart: a chunk the bridge reaches more than
/// `MAX_BEHIND` behind the newest is skipped, not abandoned, and the count
/// below is of abandoned ones.
fn render_chunks(processor: &mut IpcProcessor, chunks: usize) {
    for _ in 0..chunks {
        render_silence(processor, 32);
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// **A server that never replies costs an offline fork one budget, however
/// many chunks the render still submits.** The bridge thread waits the fork's
/// budget (here 1 s) for a reply while the fork waits on it; once the fork
/// gives up it waits no more, and the bridge returns to the few block periods
/// a live block is given. The server here publishes and replies to nothing:
/// the first chunk is abandoned at the budget, and the eight after it within
/// milliseconds each (at the budget each they would take eight seconds).
#[test]
fn a_silent_server_holds_the_bridge_for_one_budget_not_one_per_chunk() {
    let _machine = machine_lock();
    let (mut ipc, _server) = mock();
    let bridge = Arc::clone(ipc.bridge());
    let wait = ipc.watch(true);
    let (mut processor, _controller) = activate(ipc, 32);
    render_chunks(&mut processor, 2);
    assert!(
        wait.gave_up(),
        "the second render's collect misses its budget"
    );
    wait_first_abandoned(&bridge);
    let before = bridge.abandoned_replies();
    render_chunks(&mut processor, CHUNKS_AFTER);
    wait_off_the_budget(&bridge, before);
}

/// **A reactivation after a give-up stays off the budget.** The fork's wait is
/// latched across `deactivate` and `activate`: the new activation collects at
/// once, and the bridge thread must not hold one budget per chunk for a
/// server that already did not answer.
#[test]
fn a_reactivation_after_a_give_up_does_not_wait_the_budget_again() {
    let _machine = machine_lock();
    let main = main_thread();
    let (mut ipc, _server) = mock();
    let bridge = Arc::clone(ipc.bridge());
    let wait = ipc.watch(true);
    let (mut processor, controller) = activate(ipc, 32);
    render_chunks(&mut processor, 2);
    assert!(
        wait.gave_up(),
        "the second render's collect misses its budget"
    );
    wait_first_abandoned(&bridge);
    let ipc = Ipc::deactivate(main, processor, controller).expect("the halves rejoin");
    let (mut processor, _controller) = activate(ipc, 32);
    let before = bridge.abandoned_replies();
    render_chunks(&mut processor, CHUNKS_AFTER);
    wait_off_the_budget(&bridge, before);
}

/// How many times the mock server was asked for its parameter list.
fn list_asks(server: &Mock) -> usize {
    server
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|m| matches!(m, HostMessage::GetParameterList))
        .count()
}

fn param(id: u32, read_only: bool) -> ParameterInfo {
    let flags = if read_only {
        tutti_plugin_types::ParamFlags::READ_ONLY
    } else {
        tutti_plugin_types::ParamFlags::empty()
    };
    ParameterInfo::new(ParamId::new(id), "p")
        .with_flags(tutti_plugin_types::ParamFlags::READ_ONLY, flags)
}

/// **A served write is checked against the declared parameters, and the server
/// is asked once**: a read-only parameter is refused (`ReadOnly`), an undeclared
/// id too, a declared one is written, and the three writes cost one
/// `GetParameterList` between them.
#[test]
fn a_served_write_is_checked_against_one_ask() {
    use tutti_plugin_types::{Normalized, ParamWriteError};
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    *server.parameters.lock().unwrap() = vec![param(3, false), param(4, true)];
    let main = main_thread();
    let half = Normalized::of(0.5);
    let id = ParamId::new;
    assert_eq!(
        ipc.set_parameter(main, id(4), half),
        Err(ParamWriteError::ReadOnly(id(4)))
    );
    assert_eq!(
        ipc.set_parameter(main, id(9), half),
        Err(ParamWriteError::Undeclared(id(9)))
    );
    assert_eq!(ipc.set_parameter(main, id(3), half), Ok(()));
    assert_eq!(
        list_asks(&server),
        1,
        "the second and third writes asked nothing"
    );
}

/// **A resync that changes the list flips the answer, with no command to
/// carry it**: the plugin says its parameters changed on an idle bridge, the
/// bridge reads it off the connection, and the first write after the event
/// reached the listener asks again and sees the parameter that became
/// read-only.
#[test]
fn a_resync_with_a_changed_list_flips_the_answer() {
    use tutti_plugin_types::{Normalized, ParamWriteError, ResyncKind};
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    *server.parameters.lock().unwrap() = vec![param(3, false)];
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    let arrived = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heard = Arc::clone(&arrived);
    ipc.forward_events(Some(Arc::new(move |event| {
        if matches!(event, BridgeEvent::Resync(ResyncKind::ParamTitles)) {
            heard.store(true, std::sync::atomic::Ordering::Release);
        }
    })));
    *server.parameters.lock().unwrap() = vec![param(3, true)];
    server.say(&BridgeMessage::PluginParamTitlesChanged);
    let start = Instant::now();
    while !arrived.load(std::sync::atomic::Ordering::Acquire) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the event never arrived"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // The cache is forgotten before the event is forwarded: no read of the
    // bridge stands between the event and this write.
    assert_eq!(
        ipc.set_parameter(main, ParamId::new(3), half),
        Err(ParamWriteError::ReadOnly(ParamId::new(3)))
    );
    assert!(list_asks(&server) >= 2, "the list was asked again");
}

/// **A write settles the news the bridge has not delivered yet**: with the
/// bridge held inside its listener (so the plugin's change of list is on the
/// connection and the news is pending), a write to the parameter that became
/// read-only waits, and once the bridge is released it is refused `ReadOnly`
/// rather than accepted from the old list.
#[test]
fn a_write_waits_for_the_news_the_bridge_has_not_delivered() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use tutti_plugin_types::{Normalized, ParamWriteError};
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    *server.parameters.lock().unwrap() = vec![param(3, false)];
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));

    // The first event the listener sees holds the bridge thread until released.
    let (held_tx, held_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let held_tx = Mutex::new(held_tx);
    ipc.forward_events(Some(Arc::new(move |event| {
        if matches!(event, BridgeEvent::LatencyChanged { .. })
            && first.swap(false, Ordering::SeqCst)
        {
            held_tx.lock().unwrap().send(()).unwrap();
            let _ = release_rx.lock().unwrap().recv();
        }
    })));
    server.say(&BridgeMessage::LatencyChanged {
        samples: tutti_plugin_types::Samples(64),
    });
    held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the bridge reached its listener");

    // The list changes and the plugin says so, behind the held bridge.
    *server.parameters.lock().unwrap() = vec![param(3, true)];
    server.say(&BridgeMessage::PluginParamTitlesChanged);

    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let write = scope.spawn(|| {
            let answer = ipc.set_parameter(main_thread(), ParamId::new(3), half);
            done.store(true, Ordering::SeqCst);
            answer
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !done.load(Ordering::SeqCst),
            "the write was answered from the old list while news was pending"
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            write.join().unwrap(),
            Err(ParamWriteError::ReadOnly(ParamId::new(3)))
        );
    });
}

/// **A value resync does not forget the list**: every dropped parameter event
/// raises `ParamValues`, which says nothing of the declarations, so the next
/// write is still answered from the cache.
#[test]
fn a_values_resync_keeps_the_declared_list() {
    use tutti_plugin_types::{Normalized, ResyncKind};
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    *server.parameters.lock().unwrap() = vec![param(3, false)];
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    let arrived = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heard = Arc::clone(&arrived);
    ipc.forward_events(Some(Arc::new(move |event| {
        if matches!(event, BridgeEvent::Resync(ResyncKind::ParamValues)) {
            heard.store(true, std::sync::atomic::Ordering::Release);
        }
    })));
    server.say(&BridgeMessage::PluginParamValuesChanged);
    let start = Instant::now();
    while !arrived.load(std::sync::atomic::Ordering::Acquire) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the event never arrived"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    assert_eq!(list_asks(&server), 1, "the write asked nothing");
}

/// **A lost plugin is refused with `Lost`**, from the first check: a write that
/// cannot be checked is never `Ok`, and the cache is forgotten.
#[test]
fn a_lost_plugin_refuses_the_write() {
    use tutti_plugin_types::{Normalized, ParamWriteError, PluginFailure};
    let _machine = machine_lock();
    let (mut ipc, _server) = mock();
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    ipc.link.bridge.mark_crashed(PluginFailure::StreamClosed);
    assert_eq!(
        ipc.set_parameter(main, ParamId::new(3), half),
        Err(ParamWriteError::Lost(ParamId::new(3)))
    );
    assert_eq!(
        ipc.parameter_writable(main, ParamId::new(3)),
        Err(ParamWriteError::Lost(ParamId::new(3)))
    );
}

/// **A refill that began before a forgetting does not store its older answer**
/// (the generation moved): the write that asked is answered from what it read,
/// and the next one asks again.
#[test]
fn a_refill_older_than_a_forgetting_is_not_kept() {
    let cache = DeclaredCache::default();
    let list = |read_only| -> Arc<[(ParamId, ParamAccess)]> {
        vec![(ParamId::new(3), ParamAccess::reported(read_only))].into()
    };
    let Err(started) = cache.get() else {
        panic!("a new cache is empty");
    };
    // The plugin resyncs while the read is in flight.
    cache.forget();
    cache.store(started, &list(false));
    assert!(cache.get().is_err(), "the older answer was not kept");
    // A refill that began after it is kept.
    let Err(started) = cache.get() else {
        unreachable!()
    };
    cache.store(started, &list(true));
    assert_eq!(cache.get().unwrap()[0].1, ParamAccess::ReadOnly);
}
