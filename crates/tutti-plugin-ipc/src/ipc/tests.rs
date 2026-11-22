//! [`Ipc`] over a mock `plugin-server`: what its three halves answer, without a
//! plugin format or a real server. The differential test against the served
//! node, and the real-server suites, are `tutti-plugin`'s.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_rt::Mutex;

use tutti_buffers::FloatWidth;
use tutti_fixtures::{machine_lock, main_thread};
use tutti_plugin_types::{
    AnyThread, ChannelLayout, Control, Controller, EditorError, Loaded, MaxBlock, ParamAccess,
    ParameterInfo, PluginBus, PluginClass, PluginDescriptor, PluginWiring, Prepare, Processor,
    RenderMode, Reopen, Request, SampleRate, Samples,
};
use tutti_units::ParamId;

use super::DeclaredCache;
use crate::Request as WireRequest;

use crate::{
    AudioReply, AudioRequest, AudioSlab, ControlFrame, ControlReply, ControlRequest, Framed, Ipc,
    IpcController, IpcEvent, IpcParts, IpcProcessor, News, PluginBridge, ProcessGuard, RING_SLOTS,
    ReqId, SlabHandle, SlabLayout,
};

const CEILING: usize = 64;
const RATE: SampleRate = SampleRate::SR_48K;
/// The size the mock server's editor opens at.
const MOCK_EDITOR: dpi::Size = dpi::Size::Physical(dpi::PhysicalSize::new(640, 480));

/// A request the mock server was sent, on either connection.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // a test record, built once
enum Sent {
    Audio(AudioRequest),
    Control(ControlRequest),
}

/// What the mock server was sent, in order.
type Received = Arc<Log<Sent>>;

/// Writes `message` to the peer as one frame.
fn reply<T: Framed>(stream: &interprocess::local_socket::Stream, message: &T) {
    let mut stream = stream;
    crate::protocol::frame::write_frame(&mut stream, message).unwrap();
}

/// Writes the answer to request `id` to the control connection.
fn reply_control(stream: &interprocess::local_socket::Stream, id: ReqId, reply_to: &ControlReply) {
    reply(
        stream,
        &ControlFrame::Reply {
            id,
            reply: reply_to.clone(),
        },
    );
}

/// A record that a thread appends to and another waits on: an append notifies
/// the [`Wake`], so a test waits for the event it needs and never polls.
struct Log<T> {
    items: Mutex<Vec<T>>,
    appended: crate::Wake,
}

impl<T> Default for Log<T> {
    fn default() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            appended: crate::Wake::default(),
        }
    }
}

impl<T> Log<T> {
    fn push(&self, item: T) {
        self.items.lock().push(item);
        self.appended.notify();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<T>> {
        self.items.lock()
    }

    /// Whether the record satisfies `found` now.
    fn holds(&self, found: impl Fn(&[T]) -> bool) -> bool {
        found(&self.items.lock())
    }

    /// Waits until the record satisfies `found`. `false` when [`POLL_GUARD`],
    /// a hang guard far past anything the OS takes, ran out first.
    fn wait_for(&self, found: impl Fn(&[T]) -> bool) -> bool {
        self.appended
            .wait_until(crate::Wake::deadline_in(POLL_GUARD), || self.holds(&found))
    }
}

/// A served plugin over a mock server, which records what it is sent, answers
/// the asks the tests make, and sends what a test injects.
struct Mock {
    received: Received,
    /// The parameters the server declares when asked (`GetParameterList`).
    parameters: Arc<Mutex<Vec<ParameterInfo>>>,
    stream: Arc<interprocess::local_socket::Stream>,
    /// The states the server was sent, each as the bytes of the object it read.
    states: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Whether the server refuses the slabs it is asked to open.
    refuse_slabs: Arc<std::sync::atomic::AtomicBool>,
    /// Buses the plugin announces while the server is asked to open a slab,
    /// before it answers: a change that lands inside an activation.
    announce_during_setup: Arc<Mutex<Option<tutti_plugin_types::Layout>>>,
    /// The state the server saves when asked; `None` for a plugin that cannot.
    saved_state: Arc<Mutex<Option<Vec<u8>>>>,
    /// Whether the server answers a parameter-list ask with the list; when
    /// not, it answers with another kind of reply, which is no list.
    lists: Arc<std::sync::atomic::AtomicBool>,
    _bridge: crate::IpcThread,
}

impl Mock {
    /// The server says something unprompted, as a plugin does.
    fn say(&self, message: impl Into<ControlFrame>) {
        reply(&self.stream, &message.into());
    }

    /// Waits for the server's record to satisfy `found`.
    fn wait_for(&self, found: impl Fn(&[Sent]) -> bool) -> bool {
        self.received.wait_for(found)
    }
}

/// Asks the bridge something until `done`. The bridge reads what the server
/// said unprompted only when it reads a reply, so each ask is one round trip
/// to the server, which paces the loop; `POLL_GUARD` only guards a hang.
fn ask_until(what: &str, mut ask: impl FnMut(), done: impl Fn() -> bool) {
    let deadline = Instant::now() + POLL_GUARD;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        ask();
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

/// A hang guard for the tests that poll or wait on an event: far past anything
/// a healthy run needs, so a loaded machine cannot trip it. A test asserts that
/// the event came, not how soon.
const POLL_GUARD: Duration = Duration::from_secs(60);

fn mock() -> (Ipc, Mock) {
    mock_loading(
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        crate::EditorLoader::new(no_editor, tutti_fixtures::detached_work()),
    )
}

/// [`mock`] whose bridge was made for `origin` and loads its editor with
/// `load_editor`.
fn mock_loading(
    origin: tutti_plugin_types::PluginRef,
    format: tutti_plugin_types::PluginFormat,
    load_editor: crate::EditorLoader,
) -> (Ipc, Mock) {
    mock_with(origin, format, Some(load_editor))
}

/// [`mock`] whose bridge has no editor loader: its editor lives in the server.
fn mock_remote() -> (Ipc, Mock) {
    mock_with(
        tutti_plugin_types::PluginRef::file("test.clap"),
        tutti_plugin_types::PluginFormat::Clap,
        None,
    )
}

fn mock_with(
    origin: tutti_plugin_types::PluginRef,
    format: tutti_plugin_types::PluginFormat,
    load_editor: Option<crate::EditorLoader>,
) -> (Ipc, Mock) {
    use crate::transport::broker_pair;
    use std::io::Read;
    let layout = SlabLayout {
        samples_per_channel: CEILING,
        format: FloatWidth::F32,
        slots: RING_SLOTS as u32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    };
    let ((audio_stream, audio), (control_stream, stream)) = (broker_pair(), broker_pair());
    let (slab, _handle) = AudioSlab::create(layout).unwrap();
    let (bridge, thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        audio_stream,
        control_stream,
        origin,
        format,
        load_editor,
        RATE,
        Arc::new(crate::ServerWatch::new()),
        // An hour: a control request that is answered returns at once, and one
        // that is not outlasts the run's own hang guard.
        {
            let mut t = tutti_plugin_types::Timeouts::default();
            t.param = Duration::from_secs(3_600);
            t
        },
    )
    .unwrap();
    // Blocks and resets arrive on the audio connection and are only logged.
    let (audio, stream) = (Arc::new(audio), Arc::new(stream));
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
                let Ok(message) = postcard::from_bytes::<AudioRequest>(&bytes) else {
                    break;
                };
                let applied = match &message {
                    AudioRequest::ApplyParameters { id, .. } => Some(*id),
                    _ => None,
                };
                log.push(Sent::Audio(message));
                // The parameters sent between blocks are answered, as the
                // server's render thread answers them.
                if let Some(id) = applied {
                    reply(&audio, &AudioReply::ParametersApplied { id });
                }
            }
        }
    });
    let (log, served) = (Arc::clone(&received), Arc::clone(&stream));
    let states: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let stored = Arc::clone(&states);
    let answers = Arc::clone(&parameters);
    let refuse_slabs = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refusing = Arc::clone(&refuse_slabs);
    let announce_during_setup: Arc<Mutex<Option<tutti_plugin_types::Layout>>> = Arc::default();
    let announcing = Arc::clone(&announce_during_setup);
    let saved_state: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(Some(vec![1, 2, 3])));
    let saving = Arc::clone(&saved_state);
    let lists = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let listing = Arc::clone(&lists);
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
            let Ok(WireRequest { id, body: message }) = postcard::from_bytes::<WireRequest>(&bytes)
            else {
                break;
            };
            // A setup's slab and a state's object follow their frame on the stream.
            match &message {
                ControlRequest::SetupSharedMemory { slab, .. } => {
                    let received =
                        SlabHandle::receive(crate::transport::connection(&served), slab.clone());
                    if received.is_err() {
                        break;
                    }
                }
                ControlRequest::LoadState { len, blob, .. } => {
                    let state = tutti_plugin_ipc_ffi::Blob::receive(
                        crate::transport::connection(&served),
                        blob.clone(),
                    )
                    .and_then(|object| object.read(*len, tutti_plugin_types::MAX_STATE_BYTES));
                    match state {
                        Ok(bytes) => stored.lock().push(bytes),
                        Err(_) => break,
                    }
                }
                _ => {}
            }
            // Recorded before it is answered: a count read after the reply sees it.
            log.push(Sent::Control(message.clone()));
            match &message {
                ControlRequest::SetSampleRate { rate } => reply_control(
                    &served,
                    id,
                    &ControlReply::SampleRateSet {
                        rate: *rate,
                        refusal: None,
                    },
                ),
                // The server's editor: a parent of 0 is a window it cannot embed
                // into; any other opens at the size the test pins.
                ControlRequest::OpenEditor { parent, .. } => reply_control(
                    &served,
                    id,
                    &ControlReply::EditorOpened {
                        result: match parent {
                            Some(0) => Err(EditorError::ForeignWindow(0)),
                            Some(_) => Ok(Some(MOCK_EDITOR)),
                            None => Ok(None),
                        },
                    },
                ),
                ControlRequest::SetupSharedMemory { .. } => {
                    if let Some(layout) = announcing.lock().take() {
                        reply(
                            &served,
                            &ControlFrame::News(News::PluginIoChanged {
                                layout: Some(layout),
                            }),
                        );
                    }
                    reply_control(
                        &served,
                        id,
                        &if refusing.load(std::sync::atomic::Ordering::Acquire) {
                            ControlReply::Error {
                                error: tutti_plugin_types::ServerError::SlabNotOpened {
                                    reason: "the slab does not open".into(),
                                },
                            }
                        } else {
                            ControlReply::SharedMemoryReady
                        },
                    )
                }
                ControlRequest::SetEditorSize { size } => reply_control(
                    &served,
                    id,
                    &ControlReply::EditorSized { result: Ok(*size) },
                ),
                ControlRequest::LoadState { .. } => {
                    reply_control(&served, id, &ControlReply::StateLoaded { error: None })
                }
                ControlRequest::SaveState { .. } => match saving.lock().clone() {
                    Some(bytes) => {
                        let object = tutti_plugin_ipc_ffi::Blob::create(&bytes).unwrap();
                        reply_control(
                            &served,
                            id,
                            &ControlReply::StateReady {
                                len: bytes.len() as u64,
                                blob: object.wire(),
                            },
                        );
                        let _lease = object.send(crate::transport::connection(&served)).unwrap();
                    }
                    None => reply_control(
                        &served,
                        id,
                        &ControlReply::Error {
                            error: tutti_plugin_types::ServerError::Plugin(
                                "the plugin cannot save".into(),
                            ),
                        },
                    ),
                },
                ControlRequest::GetParameter { .. } => {
                    reply_control(&served, id, &ControlReply::ParameterValue { value: None })
                }
                // The settle's fence (a retune of a watched fork).
                ControlRequest::GetCurrentPreset => {
                    reply_control(&served, id, &ControlReply::CurrentPreset { id: None })
                }
                ControlRequest::GetParameterList
                    if !listing.load(std::sync::atomic::Ordering::Relaxed) =>
                {
                    reply_control(
                        &served,
                        id,
                        &ControlReply::PresetList {
                            presets: Vec::new(),
                        },
                    )
                }
                ControlRequest::GetParameterList => reply_control(
                    &served,
                    id,
                    &ControlReply::ParameterList {
                        parameters: answers.lock().clone(),
                    },
                ),
                _ => {}
            }
        }
    });
    let ipc = Ipc::new(IpcParts {
        bridge,
        replies,
        guard: Arc::new(ProcessGuard::for_test()),
        descriptor: PluginDescriptor::new("", PluginClass::Unknown),
        loaded: PluginWiring {
            inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            latency_samples: Samples::new(5),
            ..PluginWiring::default()
        },
        format: FloatWidth::F32,
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
            states,
            refuse_slabs,
            lists,
            announce_during_setup,
            saved_state,
            _bridge: thread,
        },
    )
}

fn prepare(block: usize) -> Prepare {
    Prepare::new(RATE, MaxBlock::of(Samples::new(block)))
}

fn activate(ipc: Ipc, block: usize) -> (IpcProcessor, IpcController) {
    match ipc.activate(main_thread(), prepare(block)) {
        Ok(halves) => halves,
        Err(refused) => panic!("{}", refused.reason),
    }
}

/// **A served Processor declares exactly what a chunk's MIDI holds**: the
/// inline `MidiEventVec` of the payload it fills, at any block. A node that
/// declared more would be sent events the wire drops.
#[test]
fn the_served_processor_takes_what_a_chunks_midi_holds() {
    let _machine = machine_lock();
    for block in [48, 1000] {
        let (ipc, _server) = mock();
        let (processor, _controller) = activate(ipc, block);
        let held = processor.event_input().get() as usize;
        assert_eq!(held, tutti_plugin_types::MidiEventVec::new().capacity());
        assert_eq!(held, tutti_plugin_types::MIDI_STACK_CAPACITY);
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
    assert!(processor.retune(SampleRate::SR_96K, Samples::new(60), Some(Samples::new(40))));
    assert_eq!(processor.chunk(), NonZeroUsize::new(40), "the quantum");
    assert_eq!(
        processor.latency(),
        Samples::new(40),
        "one chunk of latency"
    );
    assert!(processor.retune(SampleRate::SR_96K, Samples::new(30), None));
    assert_eq!(processor.chunk(), NonZeroUsize::new(30), "the block");
    assert!(
        received.wait_for(|log| log.iter().any(
            |m| matches!(m, Sent::Control(ControlRequest::SetSampleRate { rate }) if *rate == SampleRate::SR_96K)
        )),
        "the rate is sent"
    );
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
    let heard: Arc<Log<String>> = Arc::default();
    let sink = Arc::clone(&heard);
    ipc.forward_events(Some(Arc::new(move |event| {
        sink.push(format!("{event:?}"));
    })));
    let say = |latency| server.say(News::LatencyChanged { samples: latency });
    let main = main_thread();
    let (_processor, mut controller) = activate(ipc, 32);
    assert_eq!(controller.latency(main), Samples::new(5));
    say(Samples::new(77));
    // An idle bridge reads nothing: the next command's reply reads it.
    assert_eq!(controller.parameters(main).len(), 1);
    // The bridge's thread stores the latency, asks for the re-read, then
    // forwards the event, in that order: once the host has heard it, all
    // three are done.
    assert!(
        heard.wait_for(|log| log.iter().any(|e| e.contains("LatencyChanged"))),
        "the host hears it"
    );
    assert_eq!(controller.latency(main), Samples::new(77));
    let mut asked = Vec::new();
    controller.take_requests(main, &mut asked);
    assert_eq!(asked, [Request::LatencyChanged]);
    controller.take_requests(main, &mut asked);
    assert_eq!(asked.len(), 1, "once");
    assert!(server.wait_for(|got| {
        got.iter()
            .any(|m| matches!(m, Sent::Control(ControlRequest::GetParameterList)))
    }));
}

/// **The buses a plugin announces are the ones the host sees, and not before.**
/// The plugin has a stereo bus in and out at load; it announces a stereo main bus
/// and a mono aux bus on the way in. The controller's layout, and the `wiring`
/// its `Control` answers, follow the announcement; the slab and the pipeline
/// follow it at the next activation (the test below). A `PluginIoChanged` with
/// no layout changes nothing.
#[test]
fn the_buses_a_plugin_announces_are_the_buses_the_host_sees() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let heard: Arc<Log<String>> = Arc::default();
    let sink = Arc::clone(&heard);
    ipc.forward_events(Some(Arc::new(move |event| {
        sink.push(format!("{event:?}"));
    })));
    let main = main_thread();
    let view = ipc.controller();
    let (_processor, controller) = activate(ipc, 32);
    let at_load = controller.layout();
    assert_eq!(at_load.input_widths(), vec![ChannelLayout::STEREO]);

    server.say(News::PluginIoChanged { layout: None });
    let announced = tutti_plugin_types::Layout {
        inputs: vec![
            PluginBus::main(ChannelLayout::STEREO),
            PluginBus::new(ChannelLayout::MONO, tutti_plugin_types::BusRole::Aux),
        ],
        outputs: at_load.outputs.clone(),
    };
    server.say(News::PluginIoChanged {
        layout: Some(announced.clone()),
    });
    // An idle bridge reads nothing: the next command's reply reads it.
    assert_eq!(controller.parameters(main).len(), 1);
    ask_until(
        "the host hears it",
        || drop(controller.parameters(main)),
        || heard.holds(|log| log.iter().any(|e| e.contains("IoLayout"))),
    );
    assert_eq!(controller.layout(), announced);
    let wiring = view.wiring(main);
    assert_eq!(
        wiring.inputs, announced.inputs,
        "a view's wiring follows it"
    );
    assert_eq!(wiring.outputs, announced.outputs);
    assert_eq!(
        controller.wiring(main).inputs,
        at_load.inputs,
        "the activation's wiring is the buses it was laid out for"
    );
}

/// A mock plugin that announced a wider input and has been deactivated, with
/// the slab it was activated over: the next activation sees new buses.
fn announced_and_deactivated(ipc: Ipc, server: &Mock) -> (Ipc, tutti_plugin_types::Layout) {
    let main = main_thread();
    let heard: Arc<Log<String>> = Arc::default();
    let sink = Arc::clone(&heard);
    ipc.forward_events(Some(Arc::new(move |event| {
        sink.push(format!("{event:?}"));
    })));
    let (processor, controller) = activate(ipc, 32);
    let announced = tutti_plugin_types::Layout {
        inputs: vec![
            PluginBus::main(ChannelLayout::STEREO),
            PluginBus::new(ChannelLayout::MONO, tutti_plugin_types::BusRole::Aux),
        ],
        outputs: controller.layout().outputs,
    };
    server.say(News::PluginIoChanged {
        layout: Some(announced.clone()),
    });
    ask_until(
        "the host hears it",
        || drop(controller.parameters(main)),
        || heard.holds(|log| log.iter().any(|e| e.contains("IoLayout"))),
    );
    let ipc = Ipc::deactivate(main, processor, controller).expect("the halves rejoin");
    (ipc, announced)
}

/// The `SetupSharedMemory` messages the mock server has received.
fn slab_setups(server: &Mock) -> Vec<SlabLayout> {
    server
        .received
        .lock()
        .iter()
        .filter_map(|m| match m {
            Sent::Control(ControlRequest::SetupSharedMemory { layout, .. }) => Some(layout.clone()),
            _ => None,
        })
        .collect()
}

/// **An activation over other buses lays out a new slab for them and tells the
/// server; one over the same buses reuses the slab and tells it nothing.** The
/// slab, the pipeline and the controller's wiring come from one reading of the
/// buses.
#[test]
fn an_activation_over_new_buses_has_a_slab_laid_out_for_them() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let (ipc, announced) = announced_and_deactivated(ipc, &server);
    assert!(slab_setups(&server).is_empty(), "nothing is set up before");
    let held = ipc.slab.layout_ref().clone();
    let (processor, controller) = activate(ipc, 32);
    let setups = slab_setups(&server);
    assert_eq!(setups.len(), 1, "one new slab");
    assert_eq!(
        setups[0].inputs,
        announced.input_widths(),
        "laid out for the buses announced"
    );
    assert_ne!(setups[0], held);
    assert_eq!(controller.wiring(main_thread()).inputs, announced.inputs);

    let ipc = Ipc::deactivate(main_thread(), processor, controller).expect("rejoin");
    assert_eq!(*ipc.slab.layout_ref(), setups[0], "the new slab is kept");
    let (_p, _c) = activate(ipc, 32);
    assert_eq!(
        slab_setups(&server).len(),
        1,
        "the same buses reuse the slab"
    );
}

/// **A change of buses that lands inside an activation is the next restart's:
/// the activation's slab and wiring agree on the buses it read, and the restart
/// after it lays out the later ones.** The plugin announces a third bus while the
/// server is asked to open the slab for the second layout: the controller's
/// wiring is still the second layout (never the later one over the earlier
/// slab), the buses it has announced are the third, and the next activation
/// sets up a slab for the third and wires it. (The batcher's widths are the
/// slab's own, so they agree wherever the slab does.)
#[test]
fn a_change_inside_an_activation_waits_for_the_next_restart() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let (ipc, second) = announced_and_deactivated(ipc, &server);
    let third = tutti_plugin_types::Layout {
        inputs: vec![
            PluginBus::main(ChannelLayout::STEREO),
            PluginBus::new(ChannelLayout::MONO, tutti_plugin_types::BusRole::Aux),
            PluginBus::new(ChannelLayout::MONO, tutti_plugin_types::BusRole::Aux),
        ],
        outputs: second.outputs.clone(),
    };
    *server.announce_during_setup.lock() = Some(third.clone());
    let (processor, mut controller) = activate(ipc, 32);
    // The later layout is heard (a command's reply reads it), after the
    // activation read the second.
    ask_until(
        "the later buses are heard",
        || drop(controller.parameters(main_thread())),
        || controller.layout() == third,
    );
    assert_eq!(
        controller.wiring(main_thread()).inputs,
        second.inputs,
        "the wiring is the activation's buses, not the later ones"
    );
    let setups = slab_setups(&server);
    assert_eq!(setups.len(), 1);
    assert_eq!(
        setups[0].inputs,
        second.input_widths(),
        "the slab is the second layout"
    );
    let mut asked = Vec::new();
    controller.take_requests(main_thread(), &mut asked);
    assert!(
        asked.contains(&Request::IoChanged),
        "the announcement asks for a restart: {asked:?}"
    );

    // The restart the announcement asks for lays out the third.
    let ipc = Ipc::deactivate(main_thread(), processor, controller).expect("rejoin");
    assert_eq!(*ipc.slab.layout_ref(), setups[0]);
    let (_p, controller) = activate(ipc, 32);
    let setups = slab_setups(&server);
    assert_eq!(setups.len(), 2);
    assert_eq!(
        setups[1].inputs,
        third.input_widths(),
        "the slab follows the later layout"
    );
    assert_eq!(controller.wiring(main_thread()).inputs, third.inputs);
}

/// **A server that refuses the new slab leaves the plugin as it was, and the next
/// activation asks again.** The refusal hands back the `Ipc` with the slab it
/// held; nothing waits out a deadline for the error.
#[test]
fn a_refused_slab_leaves_the_old_one_and_the_next_activation_retries() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let (ipc, announced) = announced_and_deactivated(ipc, &server);
    let held = ipc.slab.layout_ref().clone();
    server
        .refuse_slabs
        .store(true, std::sync::atomic::Ordering::Release);
    let Err(refused) = ipc.activate(main_thread(), prepare(32)) else {
        panic!("the server refused the slab");
    };
    let ipc = refused.plugin.expect("the plugin comes back");
    assert_eq!(*ipc.slab.layout_ref(), held, "the old slab is kept");

    server
        .refuse_slabs
        .store(false, std::sync::atomic::Ordering::Release);
    let (_p, controller) = activate(ipc, 32);
    assert_eq!(controller.wiring(main_thread()).inputs, announced.inputs);
    assert_eq!(slab_setups(&server).len(), 2, "asked again, and taken");
}

/// **A refused or lost setup makes the next one happen even for the slab the
/// host holds.** The plugin goes to a second layout, the server takes that slab
/// but the host never hears (here: it refuses), and the plugin goes back to the
/// layout the host holds: the server may be on the orphan slab, so the host sets
/// the held layout up again instead of trusting its own slab.
#[test]
fn a_failed_setup_is_followed_by_one_even_for_the_layout_held() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let (ipc, _second) = announced_and_deactivated(ipc, &server);
    let held = ipc.slab.layout_ref().clone();
    server
        .refuse_slabs
        .store(true, std::sync::atomic::Ordering::Release);
    let Err(refused) = ipc.activate(main_thread(), prepare(32)) else {
        panic!("the server refused the slab");
    };
    let ipc = refused.plugin.expect("the plugin comes back");
    // The plugin is back at the buses of the slab the host holds.
    let original = tutti_plugin_types::Layout {
        inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
    };
    let view = ipc.controller();
    server.say(News::PluginIoChanged {
        layout: Some(original.clone()),
    });
    ask_until(
        "the host hears it",
        || drop(view.parameters(main_thread())),
        || view.layout() == original,
    );
    server
        .refuse_slabs
        .store(false, std::sync::atomic::Ordering::Release);
    let (_p, _c) = activate(ipc, 32);
    let setups = slab_setups(&server);
    assert_eq!(
        setups.len(),
        2,
        "a setup, though the layout is the held one"
    );
    assert_eq!(setups[1], held);
}

/// **A served plugin with no origin cannot be reopened, and says so**, through
/// both the token-free reopen and the trait's.
#[test]
fn a_plugin_with_no_origin_is_not_reopened() {
    let _machine = machine_lock();
    let (ipc, _server) = mock();
    let err = ipc.reopen_restoring(None).expect_err("no origin");
    assert!(
        matches!(err, tutti_plugin_types::IpcError::NoOrigin),
        "{err:?}"
    );
    assert!(ipc.reopen(main_thread()).is_err());
}

/// The plugin and format the last [`recording_editor`] was given.
static EDITOR_LOAD: Mutex<
    Option<(
        tutti_plugin_types::PluginRef,
        tutti_plugin_types::PluginFormat,
    )>,
> = Mutex::new(None);

fn recording_editor(
    _: tutti_plugin_types::MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    plugin: &tutti_plugin_types::PluginRef,
    format: tutti_plugin_types::PluginFormat,
) -> tutti_plugin_types::IpcResult<Box<dyn tutti_plugin_types::Editor>> {
    *EDITOR_LOAD.lock() = Some((plugin.clone(), format));
    Err(tutti_plugin_types::IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "records the choice and loads nothing".into(),
    })
}

/// **The editor-only instance loads the plugin the audio instance is**, not
/// the file's first, and as the format the audio instance was launched as,
/// whatever the file's extension: the bridge hands its loader the plugin and
/// format it was made for.
/// It needs no display (the floating open takes no window), because the loader
/// is asked before any editor exists; a real editor's identity cannot be read
/// without one, so the choice is pinned where it is made.
#[test]
fn the_editor_instance_is_loaded_with_the_choice_the_bridge_was_made_for() {
    let _machine = machine_lock();
    let chosen = tutti_plugin_types::PluginRef::new(
        "libbundle.so",
        tutti_plugin_types::PluginId::Clap("the.second.plugin".into()),
    );
    let (ipc, _peer) = mock_loading(
        chosen.clone(),
        tutti_plugin_types::PluginFormat::Clap,
        crate::EditorLoader::new(recording_editor, tutti_fixtures::detached_work()),
    );
    let refused = ipc.bridge().editor().open_floating_editor(main_thread());
    assert!(refused.is_err(), "the recording loader loads no editor");
    assert_eq!(
        *EDITOR_LOAD.lock(),
        Some((chosen, tutti_plugin_types::PluginFormat::Clap))
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
    assert_eq!(size.window, MOCK_EDITOR);
    assert!(
        size.mirrored.is_ok(),
        "a served editor has nothing to mirror ({:?})",
        size.mirrored
    );
    let wanted = dpi::Size::Physical(dpi::PhysicalSize::new(801, 601));
    assert_eq!(ipc.set_editor_size(main, wanted).expect("resized"), wanted);
    ipc.close_editor(main);

    assert!(server.wait_for(|got| matches!(
        got,
        [
            Sent::Control(ControlRequest::OpenEditor { parent: Some(0x40_0001), scale: s }),
            Sent::Control(ControlRequest::SetEditorSize { size }),
            Sent::Control(ControlRequest::CloseEditor),
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
    let _opened = ipc.open_floating_editor(main_thread()).expect("opens");
    assert!(server.wait_for(|got| matches!(
        got,
        [Sent::Control(ControlRequest::OpenEditor {
            parent: None,
            ..
        })]
    )));
}

/// **The server's refusal is the editor's error, as the server typed it.**
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
        matches!(
            refused,
            tutti_plugin_types::PluginError::Editor(EditorError::ForeignWindow(0))
        ),
        "{refused:?}"
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
    let _opened = ipc
        .open_editor(main, host_window(0x40_0001), ContentScale::IDENTITY)
        .expect("opens");
    let asked = dpi::Size::Physical(dpi::PhysicalSize::new(1024, 768));
    server.say(News::EditorWantsSize { size: asked });
    server.say(News::EditorClosed);
    // An idle bridge reads nothing: the next command's reply reads both.
    assert_eq!(
        ipc.bridge().audio().parameters().expect("answered").len(),
        1
    );

    assert_eq!(ipc.poll_editor_resize_request(main), Some(asked));
    assert_eq!(ipc.poll_editor_resize_request(main), None, "once");
    let refused = ipc
        .set_editor_size(main, asked)
        .expect_err("the server closed it");
    assert!(refused.to_string().contains("not open"), "{refused}");
    // A close of an editor the server already closed sends nothing: asked
    // after it, in order, a reply the mock answers proves what it was sent.
    ipc.close_editor(main);
    assert_eq!(
        ipc.bridge().audio().parameters().expect("answered").len(),
        1
    );
    assert!(
        !server
            .received
            .lock()
            .iter()
            .any(|m| matches!(m, Sent::Control(ControlRequest::CloseEditor))),
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
    assert_eq!(
        ipc.bridge().audio().parameters().expect("answered").len(),
        1
    );
    assert!(server.wait_for(|got| {
        got.iter()
            .any(|m| matches!(m, Sent::Control(ControlRequest::GetParameterList)))
    }));
    assert!(
        !server
            .received
            .lock()
            .iter()
            .any(|m| matches!(m, Sent::Control(ControlRequest::OpenEditor { .. }))),
        "the editor is the host's"
    );
}

/// Renders one block of `frames` silent frames through `processor`.
fn render_silence(processor: &mut IpcProcessor, frames: usize) {
    render_silence_as(processor, frames, false);
}

/// [`render_silence`], the block inside the audio thread's scope when
/// `in_scope` (where a lock, an allocation or a wait panics in a debug build).
fn render_silence_as(processor: &mut IpcProcessor, frames: usize, in_scope: bool) {
    use tutti_buffers::InputOutput;
    use tutti_plugin_types::{PluginIo, ProcessContext, ProcessOutput};
    let input = vec![vec![0.0f32; frames]; 2];
    let mut output = vec![vec![0.0f32; frames]; 2];
    let inputs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(Vec::as_mut_slice).collect();
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: InputOutput::from_slices(
            tutti_plugin_types::Samples::new(inputs[0].len()),
            &inputs,
            &mut outputs,
        )
        .expect("equal channels"),
        ctx: &ctx,
        out: &mut out,
    };
    let _scope = in_scope.then(tutti_rt::RtScope::enter);
    processor.process(&mut io).expect("the block renders");
}

/// The sequences of the `ProcessAudio` messages the mock was sent, in order.
fn process_seqs(server: &Mock, count: usize) -> Vec<u64> {
    let seqs = |log: &[Sent]| {
        log.iter()
            .filter_map(|m| match m {
                Sent::Audio(AudioRequest::ProcessAudio(data)) => Some(data.seq),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(
        server.wait_for(|log| seqs(log).len() >= count),
        "{count} chunks reach the server"
    );
    seqs(&server.received.lock())
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
/// [`CHUNKS_AFTER`] chunks, for it to be off the fork's budget. The rule that a
/// wait that gave up holds the bridge no budget any more is
/// `OfflineWait::reply_wait`, tested over its states in `offline_wait`; these
/// tests carry it through the render and the reactivation. A bridge held one
/// budget (1 s) per chunk abandons [`OFF_THE_BUDGET`] blocks in more than
/// [`POLL_GUARD`], so the hang guard fails it; one off the budget abandons them
/// in a bridge-thread turn each. The margin of [`CHUNKS_AFTER`] over
/// [`OFF_THE_BUDGET`] absorbs the chunks a descheduled bridge skips (one reached
/// more than `MAX_BEHIND` behind is skipped, not abandoned).
const OFF_THE_BUDGET: u64 = 64;
const CHUNKS_AFTER: usize = 90;

/// Asserts the bridge abandoned [`OFF_THE_BUDGET`] blocks past `before`, with
/// what it had when it did not. [`render_chunks`] has the server hold every
/// chunk before it returns, and the bridge counts a block abandoned before it
/// sends the next, so the count is settled when the render is.
fn assert_off_the_budget(bridge: &PluginBridge, before: u64) {
    let abandoned = bridge.audio().block_counts().abandoned - before;
    assert!(
        abandoned >= OFF_THE_BUDGET,
        "the bridge abandoned {abandoned} blocks after {CHUNKS_AFTER} chunks, wanted \
         {OFF_THE_BUDGET}: it is holding a block for the fork's budget"
    );
}

/// Renders `chunks` more chunks of silence through an offline fork's
/// processor, each one taken by the bridge before the next is submitted: the
/// server has the chunk's `ProcessAudio` when the bridge has finished with the
/// one before it. A chunk the bridge reaches more than `MAX_BEHIND` behind the
/// newest is skipped, not abandoned, and the count asserted is of abandoned
/// ones. A bridge held one budget per chunk fails the server's hang guard.
fn render_chunks(processor: &mut IpcProcessor, server: &Mock, chunks: usize) {
    let sent = |log: &[Sent]| {
        log.iter()
            .filter(|m| matches!(m, Sent::Audio(AudioRequest::ProcessAudio(_))))
            .count()
    };
    let mut taken = sent(&server.received.lock());
    for _ in 0..chunks {
        render_silence(processor, 32);
        taken += 1;
        assert!(
            server.wait_for(|log| sent(log) >= taken),
            "the server was sent {taken} chunks"
        );
    }
}

/// The graph's prepare for an offline render, as the node makes it: the one
/// thing that arms a watched fork's wait.
fn prepare_offline(wait: &crate::OfflineWait) {
    wait.switch().arm(true);
}

/// **A fork waits for a chunk only while its node's prepare has armed the
/// wait.** A watched fork whose graph is prepared for realtime renders chunks
/// the server never answers without waiting for one, inside a realtime scope
/// (where taking the wait's lock panics in a debug build) and without giving
/// up. Armed by an offline prepare it waits and gives up at the budget; once
/// the switch is disarmed again it stops waiting.
#[test]
fn a_fork_waits_for_a_chunk_only_when_its_graph_is_prepared_offline() {
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    let wait = ipc.watch();
    let (mut processor, _controller) = activate(ipc, 32);

    assert!(!wait.armed(), "a fork not prepared offline does not wait");
    for _ in 0..3 {
        render_silence_as(&mut processor, 32, true);
    }
    assert!(!wait.gave_up(), "a realtime fork never waited for a chunk");

    prepare_offline(&wait);
    render_chunks(&mut processor, &server, 2);
    assert!(
        wait.gave_up(),
        "an offline fork waits, and misses its budget"
    );

    wait.switch().arm(false);
    assert!(!wait.armed(), "a realtime prepare disarms it again");
}

/// **A realtime re-prepare while the Processor is away disarms a wait an
/// offline prepare armed.** The node's prepare writes the switch with no
/// Processor involved (no `retune`): the chunks the Processor renders
/// afterwards, inside the audio thread's scope, do not wait.
#[test]
fn a_realtime_reprepare_with_the_processor_away_stops_the_wait() {
    let _machine = machine_lock();
    let (mut ipc, _server) = mock();
    let wait = ipc.watch();
    let (mut processor, _controller) = activate(ipc, 32);
    prepare_offline(&wait);
    assert!(wait.armed());
    wait.switch().arm(false);
    for _ in 0..3 {
        render_silence_as(&mut processor, 32, true);
    }
    assert!(!wait.gave_up(), "no chunk was waited for");
}

/// **Telling the plugin it renders offline never arms the wait.** The render
/// mode is the plugin's own setting (an app sets it through the handle); the
/// wait is the node's, armed only by an offline prepare. A fork whose graph is
/// prepared for realtime, told `Offline` through the handle's `AnyThread` and
/// through `Control`, still renders inside the audio thread's scope without
/// waiting and never gives up, while the server hears the mode.
#[test]
fn telling_a_fork_offline_does_not_make_its_realtime_graph_wait() {
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    let wait = ipc.watch();
    let (mut processor, mut controller) = activate(ipc, 32);
    assert!(AnyThread::set_render_mode(&controller, RenderMode::Offline));
    assert!(Control::set_render_mode(
        &mut controller,
        main_thread(),
        RenderMode::Offline
    ));
    assert!(!wait.armed(), "a render mode is not the wait");
    for _ in 0..3 {
        render_silence_as(&mut processor, 32, true);
    }
    assert!(!wait.gave_up(), "no chunk was waited for");
    assert!(
        server.wait_for(|log| log.iter().any(|m| matches!(
            m,
            Sent::Control(ControlRequest::SetRenderMode {
                mode: RenderMode::Offline
            })
        ))),
        "the plugin was told offline all the same"
    );
}

/// **A server that never replies costs an offline fork one budget, however
/// many chunks the render still submits.** The bridge thread waits the fork's
/// budget for a reply while the fork waits on it; once the fork gives up it
/// waits no more, and the bridge returns to the few block periods a live block
/// is given. The server here publishes and replies to nothing: the first chunk
/// is abandoned at the budget, and every chunk after it is abandoned too.
#[test]
fn a_silent_server_holds_the_bridge_for_one_budget_not_one_per_chunk() {
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    let bridge = Arc::clone(ipc.bridge());
    let wait = ipc.watch();
    let (mut processor, _controller) = activate(ipc, 32);
    prepare_offline(&wait);
    render_chunks(&mut processor, &server, 2);
    assert!(
        wait.gave_up(),
        "the second render's collect misses its budget"
    );
    assert!(
        bridge.audio().block_counts().abandoned >= 1,
        "the first block is abandoned at the budget"
    );
    let before = bridge.audio().block_counts().abandoned;
    render_chunks(&mut processor, &server, CHUNKS_AFTER);
    assert_off_the_budget(&bridge, before);
}

/// **A reactivation after a give-up stays off the budget.** The fork's wait is
/// latched across `deactivate` and `activate`: the new activation collects at
/// once, and the bridge thread must not hold one budget per chunk for a
/// server that already did not answer.
#[test]
fn a_reactivation_after_a_give_up_does_not_wait_the_budget_again() {
    let _machine = machine_lock();
    let main = main_thread();
    let (mut ipc, server) = mock();
    let bridge = Arc::clone(ipc.bridge());
    let wait = ipc.watch();
    let (mut processor, controller) = activate(ipc, 32);
    prepare_offline(&wait);
    render_chunks(&mut processor, &server, 2);
    assert!(
        wait.gave_up(),
        "the second render's collect misses its budget"
    );
    assert!(
        bridge.audio().block_counts().abandoned >= 1,
        "the first block is abandoned at the budget"
    );
    let ipc = Ipc::deactivate(main, processor, controller).expect("the halves rejoin");
    let (mut processor, _controller) = activate(ipc, 32);
    let before = bridge.audio().block_counts().abandoned;
    render_chunks(&mut processor, &server, CHUNKS_AFTER);
    assert_off_the_budget(&bridge, before);
}

/// How many times the mock server was asked for its parameter list.
fn list_asks(server: &Mock) -> usize {
    server
        .received
        .lock()
        .iter()
        .filter(|m| matches!(m, Sent::Control(ControlRequest::GetParameterList)))
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
    use tutti_plugin_types::ParamWriteError;
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    *server.parameters.lock() = vec![param(3, false), param(4, true)];
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
    use tutti_plugin_types::{ParamWriteError, ResyncKind};
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    *server.parameters.lock() = vec![param(3, false)];
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    let arrived: Arc<Log<()>> = Arc::default();
    let heard = Arc::clone(&arrived);
    ipc.forward_events(Some(Arc::new(move |event| {
        if matches!(event, IpcEvent::Resync(ResyncKind::ParamTitles)) {
            heard.push(());
        }
    })));
    *server.parameters.lock() = vec![param(3, true)];
    server.say(News::PluginParamTitlesChanged);
    assert!(
        arrived.wait_for(|log| !log.is_empty()),
        "the event never arrived"
    );
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
/// rather than accepted from the old list, and puts nothing on the wire.
///
/// The limit: the release follows the write's start, not its arrival in the
/// settle wait. A release that lands before the write reads the list lets the
/// news settle first, and a write that did not wait would then pass too; the
/// test fails such a design only when the write reads the list first.
#[test]
fn a_write_waits_for_the_news_the_bridge_has_not_delivered() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use tutti_plugin_types::ParamWriteError;
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let sets = |got: &[Sent]| {
        got.iter()
            .filter(|m| matches!(m, Sent::Audio(AudioRequest::ApplyParameters { .. })))
            .count()
    };
    let (mut ipc, server) = mock();
    *server.parameters.lock() = vec![param(3, false)];
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    // A read of the plugin sends what no block has carried, ahead of itself.
    let _ = ipc.parameter(main, ParamId::new(3));
    assert!(
        server.wait_for(|got| sets(got) == 1),
        "the first write reaches the server"
    );

    // The first event the listener sees holds the bridge thread until released.
    let (held_tx, held_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let held_tx = Mutex::new(held_tx);
    ipc.forward_events(Some(Arc::new(move |event| {
        if matches!(event, IpcEvent::LatencyChanged { .. }) && first.swap(false, Ordering::SeqCst) {
            held_tx.lock().send(()).unwrap();
            let _ = release_rx.lock().recv();
        }
    })));
    server.say(News::LatencyChanged {
        samples: tutti_plugin_types::Samples::new(64),
    });
    held_rx
        .recv_timeout(POLL_GUARD)
        .expect("the bridge reached its listener");

    // The list changes and the plugin says so, behind the held bridge.
    *server.parameters.lock() = vec![param(3, true)];
    server.say(News::PluginParamTitlesChanged);

    std::thread::scope(|scope| {
        let (calling_tx, calling_rx) = mpsc::channel::<()>();
        let ipc = &mut ipc;
        let write = scope.spawn(move || {
            calling_tx.send(()).unwrap();
            ipc.set_parameter(main_thread(), ParamId::new(3), half)
        });
        // The bridge is released once the write is on its way: a write answered
        // from the old list is `Ok`, and the refusal below is the settled one.
        calling_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(
            write.join().unwrap(),
            Err(ParamWriteError::ReadOnly(ParamId::new(3)))
        );
    });
    // The control stream is ordered: once a round trip has come back, a write
    // the refused one had put on it would be on the record.
    let _ = ipc.parameter(main, ParamId::new(3));
    assert_eq!(
        sets(&server.received.lock()),
        1,
        "the refused write reached the server"
    );
}

/// **A value resync does not forget the list**: every dropped parameter event
/// raises `ParamValues`, which says nothing of the declarations, so the next
/// write is still answered from the cache.
#[test]
fn a_values_resync_keeps_the_declared_list() {
    use tutti_plugin_types::ResyncKind;
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    *server.parameters.lock() = vec![param(3, false)];
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    let arrived: Arc<Log<()>> = Arc::default();
    let heard = Arc::clone(&arrived);
    ipc.forward_events(Some(Arc::new(move |event| {
        if matches!(event, IpcEvent::Resync(ResyncKind::ParamValues)) {
            heard.push(());
        }
    })));
    server.say(News::PluginParamValuesChanged);
    assert!(
        arrived.wait_for(|log| !log.is_empty()),
        "the event never arrived"
    );
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    assert_eq!(list_asks(&server), 1, "the write asked nothing");
}

/// **A lost plugin is refused with `Lost`**, from the first check: a write that
/// cannot be checked is never `Ok`, and the cache is forgotten.
#[test]
fn a_lost_plugin_refuses_the_write() {
    use tutti_plugin_types::{ParamWriteError, PluginFailure};
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (mut ipc, _server) = mock();
    let main = main_thread();
    let half = Normalized::of(0.5);
    assert_eq!(ipc.set_parameter(main, ParamId::new(3), half), Ok(()));
    ipc.link
        .bridge
        .audio()
        .mark_crashed(PluginFailure::StreamClosed);
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

/// Submits one block of silence by hand, and returns the parameter changes the
/// server was sent with it (the block with sequence `seq`).
fn changes_sent_with_block(
    ipc: &Ipc,
    server: &Mock,
    seq: u64,
) -> tutti_plugin_types::ParameterChanges {
    let accepted = ipc.bridge().audio().submit(
        seq,
        32,
        Default::default(),
        &mut tutti_plugin_types::ParameterChanges::new(),
        Default::default(),
        Default::default(),
        tutti_plugin_types::TransportInfo::new(RATE),
        Default::default(),
    );
    assert!(accepted.accepted());
    let sent = |log: &[Sent]| {
        log.iter()
            .any(|m| matches!(m, Sent::Audio(AudioRequest::ProcessAudio(d)) if d.seq == seq))
    };
    assert!(server.wait_for(sent), "block {seq} reached the server");
    server
        .received
        .lock()
        .iter()
        .find_map(|m| match m {
            Sent::Audio(AudioRequest::ProcessAudio(d)) if d.seq == seq => {
                Some(d.param_changes.clone())
            }
            _ => None,
        })
        .expect("the block was sent")
}

/// The last value a block's changes carry for `id`.
fn last_change(changes: &tutti_plugin_types::ParameterChanges, id: ParamId) -> Option<f64> {
    changes
        .queues
        .iter()
        .find(|q| q.param_id == id)
        .and_then(|q| q.points.last())
        .map(|p| p.value.get())
}

/// **Writes do not drop for lack of room, and the latest to each parameter
/// reaches the plugin with the next block.** Three times the control queue's
/// room in writes, round-robin over three parameters, are made before any block
/// is sent; the block carries each parameter's last value, on its first frame.
#[test]
fn many_served_writes_before_a_block_leave_each_parameter_at_its_latest() {
    use tutti_plugin_types::AnyThread;
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (ipc, server) = mock();
    *server.parameters.lock() = vec![param(1, false), param(2, false), param(3, false)];
    let ids = [ParamId::new(1), ParamId::new(2), ParamId::new(3)];
    let writes = 3 * crate::client::audio::COMMAND_QUEUE_SIZE;
    for n in 0..writes {
        let id = ids[n % 3];
        let value = Normalized::of(n as f64 / writes as f64);
        assert_eq!(AnyThread::set_parameter_value(&ipc, id, value), Ok(()));
    }
    let changes = changes_sent_with_block(&ipc, &server, 1);
    for (k, id) in ids.iter().enumerate() {
        let last = writes - 3 + k;
        assert_eq!(
            last_change(&changes, *id),
            Some(last as f64 / writes as f64),
            "{id:?}"
        );
        let queue = changes.queues.iter().find(|q| q.param_id == *id).unwrap();
        assert_eq!(
            queue.points.len(),
            1,
            "one value per parameter, not a queue"
        );
        assert_eq!(queue.points[0].sample_offset, 0, "on the first frame");
    }
}

/// **A write to a parameter the plugin does not declare fails at the call**,
/// naming it, from the table the first write built: the plugin's list is asked
/// once, not once per write.
#[test]
fn a_served_write_to_an_unknown_parameter_is_unknown_at_the_call_without_asking_again() {
    use tutti_plugin_types::{AnyThread, UnknownParam};
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (ipc, server) = mock();
    *server.parameters.lock() = vec![param(3, false)];
    let half = Normalized::of(0.5);
    assert_eq!(
        AnyThread::set_parameter_value(&ipc, ParamId::new(3), half),
        Ok(())
    );
    let asks = list_asks(&server);
    for _ in 0..5 {
        assert_eq!(
            AnyThread::set_parameter_value(&ipc, ParamId::new(9), half),
            Err(UnknownParam(ParamId::new(9)))
        );
    }
    assert_eq!(
        list_asks(&server),
        asks,
        "no write asked for the list again"
    );
}

/// **A write queued before a state load never lands after it**: the load
/// replaced the values the write was made against, so the next block carries
/// nothing for it, and a write made after the load does.
#[test]
fn a_served_write_queued_before_a_state_load_does_not_land_after_it() {
    use tutti_plugin_types::{AnyThread, StateUse};
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (ipc, server) = mock();
    *server.parameters.lock() = vec![param(1, false), param(2, false)];
    assert_eq!(
        AnyThread::set_parameter_value(&ipc, ParamId::new(1), Normalized::of(0.25)),
        Ok(())
    );
    assert!(ipc.load_state(&[1, 2, 3], StateUse::Project).is_ok());
    assert_eq!(
        AnyThread::set_parameter_value(&ipc, ParamId::new(2), Normalized::of(0.75)),
        Ok(())
    );
    let changes = changes_sent_with_block(&ipc, &server, 1);
    assert_eq!(
        last_change(&changes, ParamId::new(1)),
        None,
        "the stale write"
    );
    assert_eq!(last_change(&changes, ParamId::new(2)), Some(0.75));
}

/// **A write drained into a block pushed before a load lands before the load,
/// never after it**: the audio lane sends the block, and the control lane's load
/// waits for it (the stamp every control command carries), so the server has
/// the write when it takes the load, and what the load replaces is not
/// overwritten by it.
#[test]
fn a_write_in_a_block_pushed_before_a_load_reaches_the_server_before_the_load() {
    use tutti_plugin_types::{AnyThread, StateUse};
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (ipc, server) = mock();
    *server.parameters.lock() = vec![param(1, false)];
    assert_eq!(
        AnyThread::set_parameter_value(&ipc, ParamId::new(1), Normalized::of(0.25)),
        Ok(())
    );
    // The block carries the write (it is drained when the lane sends it), and
    // the load is asked for once the lane has drained it.
    let _ = changes_sent_with_block(&ipc, &server, 1);
    assert!(ipc.load_state(&[1], StateUse::Project).is_ok());
    let log = server.received.lock();
    let block = log
        .iter()
        .position(|m| matches!(m, Sent::Audio(AudioRequest::ProcessAudio(d)) if d.seq == 1))
        .expect("the block was sent");
    let load = log
        .iter()
        .position(|m| matches!(m, Sent::Control(ControlRequest::LoadState { .. })))
        .expect("the load was sent");
    assert!(block < load, "the block, and its write, came first");
    let Sent::Audio(AudioRequest::ProcessAudio(data)) = &log[block] else {
        unreachable!()
    };
    assert_eq!(
        last_change(&data.param_changes, ParamId::new(1)),
        Some(0.25)
    );
}

/// **A state loaded through the served plugin reaches the server as one object
/// of its own length**, past any size a frame carries: the server reads back
/// the bytes the host sent, and the load is answered.
#[test]
fn a_loaded_state_reaches_the_server_as_one_object() {
    use tutti_plugin_types::StateUse;
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let state: Vec<u8> = (0..crate::CONTROL_FRAME_BYTES + 777)
        .map(|i| (i % 251) as u8)
        .collect();
    assert!(ipc.load_state(&state, StateUse::Project).is_ok());
    let states = server.states.lock();
    assert_eq!(states.len(), 1, "one object for the one load");
    assert!(
        states[0] == state,
        "the server read other bytes than were sent"
    );
}

/// **A read after writes no block has carried sends them on the audio lane,
/// in order with the blocks**: a block pushed before the write reaches the
/// server first, and the parameters follow it as their own message, before the
/// request that asked.
#[test]
fn a_flush_rides_the_audio_lane_behind_the_blocks_pushed_before_it() {
    use tutti_plugin_types::AnyThread;
    use tutti_units::Normalized;
    let _machine = machine_lock();
    let (ipc, server) = mock();
    *server.parameters.lock() = vec![param(1, false)];
    let _ = changes_sent_with_block(&ipc, &server, 1);
    assert_eq!(
        AnyThread::set_parameter_value(&ipc, ParamId::new(1), Normalized::of(0.5)),
        Ok(())
    );
    let _ = ipc.parameter(main_thread(), ParamId::new(1));
    let log = server.received.lock();
    let block = log
        .iter()
        .position(|m| matches!(m, Sent::Audio(AudioRequest::ProcessAudio(_))))
        .expect("the block");
    let flush = log
        .iter()
        .position(|m| matches!(m, Sent::Audio(AudioRequest::ApplyParameters { .. })))
        .expect("the parameters sent between blocks");
    let read = log
        .iter()
        .position(|m| matches!(m, Sent::Control(ControlRequest::GetParameter { .. })))
        .expect("the read");
    assert!(block < flush && flush < read, "{block} {flush} {read}");
    let Sent::Audio(AudioRequest::ApplyParameters { param_changes, .. }) = &log[flush] else {
        unreachable!()
    };
    assert_eq!(last_change(param_changes, ParamId::new(1)), Some(0.5));
}

/// The editor instance's mirror, on a scripted stand-in for the plugin's
/// editor: what the bridge's editor site counts and reports is the bridge's,
/// and what each format's editor does is its host crate's.
mod editor_mirror {
    use super::*;
    use tutti_plugin_types::{
        EditorCapabilities, EditorMirrorError, EditorOpened, EditorWindow, ParamEdit, PluginError,
        PluginResult, StateError, WindowHandle,
    };
    use tutti_units::Normalized;

    /// What the next editor the loader makes does.
    #[derive(Default)]
    struct Script {
        /// The editor refuses the state it is given.
        refuses_state: bool,
        /// The editor cannot say its own state.
        cannot_save: bool,
        /// A parameter edit the user made, which `poll_edits` yields once.
        edit: Option<ParamEdit>,
        /// The requests the editor makes, once.
        requests: Vec<Request>,
        /// The editor reports its state changed, once.
        dirty: bool,
    }

    static SCRIPT: Mutex<Option<Script>> = Mutex::new(None);

    struct Scripted(Script);

    impl EditorWindow for Scripted {
        fn has_editor(&self, _: tutti_plugin_types::MainThreadMarker) -> bool {
            true
        }
        fn open_editor(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
            _: WindowHandle,
            _: tutti_plugin_types::ContentScale,
        ) -> PluginResult<EditorOpened<dpi::Size>> {
            Ok(EditorOpened::new(MOCK_EDITOR))
        }
        fn open_floating_editor(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
        ) -> PluginResult<EditorOpened<()>> {
            Ok(EditorOpened::new(()))
        }
        fn close_editor(&mut self, _: tutti_plugin_types::MainThreadMarker) {}
        fn editor_idle(&mut self, _: tutti_plugin_types::MainThreadMarker) {}
        fn editor_capabilities(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
        ) -> EditorCapabilities {
            EditorCapabilities::default()
        }
        fn set_editor_size(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
            requested: dpi::Size,
        ) -> PluginResult<dpi::Size> {
            Ok(requested)
        }
        fn poll_editor_resize_request(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
        ) -> Option<dpi::Size> {
            None
        }
    }

    impl tutti_plugin_types::Editor for Scripted {
        fn set_parameter(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
            _: ParamId,
            _: Normalized,
        ) {
        }
        fn set_state(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
            _: &[u8],
        ) -> PluginResult<()> {
            if self.0.refuses_state {
                return Err(PluginError::StateFailed(StateError::Rejected(
                    "no state for this editor".into(),
                )));
            }
            Ok(())
        }
        fn poll_edits(&mut self, _: tutti_plugin_types::MainThreadMarker) -> Vec<ParamEdit> {
            self.0.edit.take().into_iter().collect()
        }
        fn save_state(&mut self, _: tutti_plugin_types::MainThreadMarker) -> PluginResult<Vec<u8>> {
            if self.0.cannot_save {
                return Err(PluginError::StateFailed(StateError::Unavailable));
            }
            Ok(vec![9, 9])
        }
        fn take_dirty(&mut self, _: tutti_plugin_types::MainThreadMarker) -> bool {
            std::mem::take(&mut self.0.dirty)
        }
        fn take_requests(
            &mut self,
            _: tutti_plugin_types::MainThreadMarker,
            out: &mut Vec<Request>,
        ) {
            out.append(&mut self.0.requests);
        }
    }

    fn scripted(
        _: tutti_plugin_types::MainThreadMarker,
        _: &tutti_plugin_types::MainWorkHandle,
        _: &tutti_plugin_types::PluginRef,
        _: tutti_plugin_types::PluginFormat,
    ) -> tutti_plugin_types::IpcResult<Box<dyn tutti_plugin_types::Editor>> {
        let script = SCRIPT.lock().take().unwrap_or_default();
        Ok(Box::new(Scripted(script)))
    }

    /// A served plugin whose editor is `script`'s.
    fn with_editor(script: Script) -> (Ipc, Mock) {
        *SCRIPT.lock() = Some(script);
        mock_loading(
            tutti_plugin_types::PluginRef::file("test.clap"),
            tutti_plugin_types::PluginFormat::Clap,
            crate::EditorLoader::new(scripted, tutti_fixtures::detached_work()),
        )
    }

    fn refused(ipc: &Ipc) -> u64 {
        ipc.bridge()
            .editor()
            .mirror_transfers_refused_cell()
            .expect("an in-process editor has a mirror")
            .get()
    }

    /// **An editor given the audio instance's state reports it mirrored**,
    /// and counts nothing.
    #[test]
    fn an_editor_given_the_state_is_mirrored() {
        let _machine = machine_lock();
        let (ipc, _server) = with_editor(Script::default());
        let opened = ipc
            .bridge()
            .editor()
            .open_floating_editor(main_thread())
            .expect("opens");
        assert!(opened.mirrored.is_ok(), "{:?}", opened.mirrored);
        assert_eq!(refused(&ipc), 0);
    }

    /// **An editor that refuses the state still opens, and says so**: the open
    /// carries the refusal and the mirror's count moves.
    #[test]
    fn an_editor_that_refuses_the_state_opens_and_reports_it() {
        let _machine = machine_lock();
        let (ipc, _server) = with_editor(Script {
            refuses_state: true,
            ..Script::default()
        });
        let opened = ipc
            .bridge()
            .editor()
            .open_floating_editor(main_thread())
            .expect("the editor still opens");
        assert!(
            matches!(opened.mirrored, Err(EditorMirrorError::Refused(_))),
            "{:?}",
            opened.mirrored
        );
        assert_eq!(refused(&ipc), 1);
    }

    /// **A state the audio instance cannot give leaves the editor at its
    /// defaults, and the embedded open says so.**
    #[test]
    fn an_unreadable_audio_state_is_reported_by_the_open() {
        let _machine = machine_lock();
        let (ipc, server) = with_editor(Script::default());
        *server.saved_state.lock() = None;
        let opened = ipc
            .bridge()
            .editor()
            .open_editor(
                main_thread(),
                tutti_fixtures::StandInWindow.handle(),
                tutti_plugin_types::ContentScale::IDENTITY,
            )
            .expect("the editor still opens");
        assert_eq!(opened.window, MOCK_EDITOR);
        assert!(
            matches!(opened.mirrored, Err(EditorMirrorError::Unread(_))),
            "{:?}",
            opened.mirrored
        );
        assert_eq!(refused(&ipc), 1);
    }

    /// **An edit the audio instance has no parameter for is counted**, not
    /// dropped silently: the editor reports a change to an id the plugin never
    /// listed.
    #[test]
    fn an_edit_the_audio_instance_refuses_is_counted() {
        let _machine = machine_lock();
        let (ipc, _server) = with_editor(Script {
            edit: Some(ParamEdit::Change {
                id: ParamId::new(404),
                value: Normalized::of(0.5),
                record: true,
            }),
            ..Script::default()
        });
        let editor = ipc.bridge().editor();
        let _opened = editor.open_floating_editor(main_thread()).expect("opens");
        assert_eq!(refused(&ipc), 0);
        editor.editor_idle(main_thread());
        assert_eq!(refused(&ipc), 1);
    }

    /// **A state the editor cannot read is counted** twice over: at the open,
    /// where it leaves no baseline, and when it is pulled to the audio
    /// instance.
    #[test]
    fn an_editor_state_that_cannot_be_read_is_counted() {
        let _machine = machine_lock();
        let (ipc, _server) = with_editor(Script {
            cannot_save: true,
            dirty: true,
            ..Script::default()
        });
        let editor = ipc.bridge().editor();
        let _opened = editor.open_floating_editor(main_thread()).expect("opens");
        assert_eq!(refused(&ipc), 1, "the baseline the open could not take");
        editor.editor_idle(main_thread());
        assert_eq!(refused(&ipc), 2, "the pull");
    }

    /// **A state the editor refuses on a later mirror is counted**, beside the
    /// open's own.
    #[test]
    fn a_later_mirror_the_editor_refuses_is_counted() {
        let _machine = machine_lock();
        let (ipc, _server) = with_editor(Script {
            refuses_state: true,
            ..Script::default()
        });
        let editor = ipc.bridge().editor();
        let _opened = editor.open_floating_editor(main_thread()).expect("opens");
        assert_eq!(refused(&ipc), 1, "the open's");
        editor.mirror_state(main_thread(), &[1]);
        assert_eq!(refused(&ipc), 2);
    }

    /// **A request the editor instance makes that no one can answer is
    /// counted**; a transport request is the application's and is not.
    #[test]
    fn a_dropped_editor_request_is_counted() {
        let _machine = machine_lock();
        let (ipc, _server) = with_editor(Script {
            requests: vec![Request::Restart, Request::LatencyChanged],
            ..Script::default()
        });
        let editor = ipc.bridge().editor();
        let dropped = editor
            .editor_requests_dropped_cell()
            .expect("an in-process editor");
        let _opened = editor.open_floating_editor(main_thread()).expect("opens");
        assert_eq!(dropped.get(), 0);
        editor.editor_idle(main_thread());
        assert_eq!(dropped.get(), 2);
    }
}

/// **A server that gives no parameter list fails the launch's list step**, with
/// the error that says so, rather than a plugin with an empty list.
#[test]
fn a_server_that_gives_no_parameter_list_is_unlisted() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    server
        .lists
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let err = super::listed_parameters(ipc.bridge()).expect_err("no list came");
    assert!(
        matches!(err, tutti_plugin_types::IpcError::ParametersUnlisted),
        "{err:?}"
    );
}

/// **A plugin with no parameters lists none, which is an answer**: the load's
/// list step counts zero and does not fail.
#[test]
fn a_plugin_with_no_parameters_lists_none_and_loads() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    server.parameters.lock().clear();
    assert_eq!(super::listed_parameters(ipc.bridge()).ok(), Some(0));
}
