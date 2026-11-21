//! The plugin handle over a served plugin, against a hand-written mock
//! `plugin-server` on the wire: what the handle's round-trips carry and how it
//! answers a server that is silent, late or unsolicited. They live here because
//! the wire is this crate's, and `tutti-plugin-host`, which holds the handle,
//! names no hosting.

#[path = "../../tutti-plugin-host/tests/support/param_reads.rs"]
mod param_reads;
use param_reads::ParamReads as _;
#[path = "../../tutti-plugin-host/tests/support/param_list.rs"]
mod param_list;
use crate::AudioSlab;
use crate::PluginBridge;
use crate::SlabLayout;
use crate::unique_socket_path;
use crate::{
    AudioReply, AudioRequest, ControlFrame, ControlReply, ControlRequest, Framed, News,
    PROTOCOL_VERSION, Request,
};
use crate::{IpcEvent, IpcThread};
use param_list::ParamList as _;
use std::sync::Arc;
use tutti_buffers::FloatWidth;
use tutti_fixtures::main_thread;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{
    ChannelLayout, Features, LoadedPlugin, ParameterInfo, PluginBus, PluginDescriptor, PluginTail,
    Samples,
};
use tutti_plugin_types::{EditorPresence, PluginClass};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::SampleRate;

/// The editor loader of a plugin that has none to load: every load fails by name.
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

/// A handle over `bridge` as a served plugin's control serves it, with a guard
/// that owns no subprocess.
fn handle_over(
    bridge: Arc<PluginBridge>,
    slab: crate::SlabWriter,
    replies: crate::Replies,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
) -> PluginHandle {
    let guard = Arc::new(crate::ProcessGuard::for_test(
        crate::unique_socket_path().expect("the OS has a random source"),
    ));
    let ipc = crate::Ipc::new(crate::IpcParts {
        bridge,
        guard,
        descriptor: descriptor.clone(),
        loaded: loaded.clone(),
        format: FloatWidth::F32,
        ceiling: 512,
        slab,
        replies,
        rate: SampleRate::SR_48K,
        parameters: 0,
        timeout: std::time::Duration::from_secs(5),
        origin: None,
    });
    let served = Arc::new(ipc.controller());
    PluginHandle::from_backend(
        Arc::clone(&served),
        Some(served),
        descriptor,
        loaded,
        Default::default(),
    )
}

/// Waits until `seen` holds, polling: a hang guard of a minute, not a speed claim.
fn eventually(what: &str, mut seen: impl FnMut() -> bool) {
    let guard = tutti_platform::Instant::now() + std::time::Duration::from_secs(60);
    while !seen() {
        assert!(
            tutti_platform::Instant::now() < guard,
            "never happened: {what}"
        );
        std::thread::yield_now();
    }
}

/// Read a request from an interprocess socket (blocking).
fn recv_host_msg<T: Framed>(stream: &interprocess::local_socket::Stream) -> T {
    use std::io::Read;
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).unwrap();
    postcard::from_bytes(&buf).unwrap()
}

/// Send a reply to an interprocess socket (blocking).
fn send_bridge_msg<T: Framed>(stream: &interprocess::local_socket::Stream, msg: &T) {
    let mut stream = stream;
    crate::write_frame(&mut stream, msg).unwrap();
}

/// What a scripted server says for a request: its answer, or news.
enum Say {
    Reply(ControlReply),
    News(News),
    /// A state: its `StateReady` frame and the object that follows it.
    State(Vec<u8>),
}

impl From<ControlReply> for Say {
    fn from(reply: ControlReply) -> Self {
        Self::Reply(reply)
    }
}

impl From<News> for Say {
    fn from(news: News) -> Self {
        Self::News(news)
    }
}

/// Accepts the host's audio connection and then its control one, as the
/// server does (the host connects the second after the first one's `Ready`),
/// and serves the control one on its own thread with `lane`. The audio
/// connection is read and not answered. The returned handle is the audio
/// lane's, which ends with the host; the socket file is removed then.
fn serve_lanes(
    listener: &interprocess::local_socket::Listener,
    path: std::path::PathBuf,
    lane: impl Fn(&interprocess::local_socket::Stream) + Send + Sync + 'static,
) -> std::thread::JoinHandle<()> {
    use interprocess::local_socket::traits::Listener as _;
    let audio = listener.accept().unwrap();
    send_bridge_msg(
        &audio,
        &AudioReply::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let control = listener.accept().unwrap();
    send_bridge_msg(
        &control,
        &ControlFrame::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    std::thread::Builder::new()
        .name("mock-plugin-server-control".to_string())
        .spawn(move || lane(&control))
        .unwrap();
    std::thread::Builder::new()
        .name("mock-plugin-server".to_string())
        .spawn(move || {
            struct Cleanup(std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _cleanup = Cleanup(path);
            while std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                recv_host_msg::<AudioRequest>(&audio)
            }))
            .is_ok()
            {}
        })
        .unwrap()
}

/// Create a `PluginHandle` backed by a responsive mock server. The
/// `respond` callback is invoked for each inbound `ControlRequest`; returning
/// `Some(..)` sends that answer back, `None` silently drops.
fn handle_with_mock_server(
    respond: impl Fn(ControlRequest) -> Option<ControlReply> + Send + 'static,
) -> (PluginHandle, IpcThread, std::thread::JoinHandle<()>) {
    use interprocess::local_socket::ListenerOptions;

    let path = unique_socket_path().expect("the OS has a random source");
    let _ = std::fs::remove_file(&path);
    let name = crate::socket_name(&path).unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    // The test writes no audio: the slab is kept for the served plugin it builds.
    let (slab, _handle) = AudioSlab::create(SlabLayout {
        slots: crate::RING_SLOTS as u32,
        samples_per_channel: 512,
        format: FloatWidth::F32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    })
    .unwrap();
    let (bridge, bridge_thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        path.clone(),
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(crate::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        SampleRate::SR_48K,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();

    let respond = std::sync::Mutex::new(respond);
    let server_thread = serve_lanes(&listener, path, move |stream| {
        loop {
            let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                recv_host_msg::<Request>(stream)
            })) {
                Ok(msg) => msg,
                Err(_) => break,
            };
            let response = (respond.lock().unwrap())(msg.body);
            if let Some(reply) = response
                && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    send_bridge_msg(stream, &ControlFrame::Reply { id: msg.id, reply })
                }))
                .is_err()
            {
                break;
            }
        }
    });

    let descriptor = PluginDescriptor {
        name: "Test Plugin".into(),
        vendor: String::new(),
        version: tutti_plugin_types::PluginVersion::Absent,
        class: PluginClass::Unknown,
        editor: EditorPresence::Present,
        measured: None,
        widths: None,
    };
    let loaded = LoadedPlugin {
        inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        latency_samples: Samples::ZERO,
        tail: PluginTail::Unknown,
        features: Features::EDITOR,
        probed: Features::EDITOR,
        ..LoadedPlugin::default()
    };
    let plugin_handle = handle_over(bridge, slab, replies, descriptor, loaded);

    (plugin_handle, bridge_thread, server_thread)
}

/// Variant that (1) returns the `PluginBridge` so tests can install a
/// listener, and (2) lets `respond` emit zero-or-more messages per
/// request — which is what allows scripting trailing unsolicited events.
fn handle_with_multi_reply_server(
    respond: impl Fn(ControlRequest) -> Vec<Say> + Send + 'static,
) -> (
    PluginHandle,
    Arc<PluginBridge>,
    IpcThread,
    std::thread::JoinHandle<()>,
) {
    use interprocess::local_socket::ListenerOptions;

    let path = unique_socket_path().expect("the OS has a random source");
    let _ = std::fs::remove_file(&path);
    let name = crate::socket_name(&path).unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    // The test writes no audio: the slab is kept for the served plugin it builds.
    let (slab, _handle) = AudioSlab::create(SlabLayout {
        slots: crate::RING_SLOTS as u32,
        samples_per_channel: 512,
        format: FloatWidth::F32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    })
    .unwrap();
    let (bridge, bridge_thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        path.clone(),
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(crate::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        SampleRate::SR_48K,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();

    let respond = std::sync::Mutex::new(respond);
    let server_thread = serve_lanes(&listener, path, move |stream| {
        loop {
            let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                recv_host_msg::<Request>(stream)
            })) {
                Ok(msg) => msg,
                Err(_) => break,
            };
            let replies = (respond.lock().unwrap())(msg.body);
            for said in replies {
                let (frame, object) = match said {
                    Say::Reply(reply) => (ControlFrame::Reply { id: msg.id, reply }, None),
                    Say::News(news) => (ControlFrame::News(news), None),
                    Say::State(bytes) => {
                        let object = tutti_plugin_ipc_ffi::Blob::create(&bytes).unwrap();
                        let reply = ControlReply::StateReady {
                            len: bytes.len() as u64,
                            blob: object.wire(),
                        };
                        (ControlFrame::Reply { id: msg.id, reply }, Some(object))
                    }
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    send_bridge_msg(stream, &frame);
                    if let Some(object) = object {
                        let _lease = object.send(crate::transport::connection(stream)).unwrap();
                    }
                }))
                .is_err()
                {
                    return;
                }
            }
        }
    });

    let descriptor = PluginDescriptor {
        name: "Test Plugin".into(),
        vendor: String::new(),
        version: tutti_plugin_types::PluginVersion::Absent,
        class: PluginClass::Unknown,
        editor: EditorPresence::Present,
        measured: None,
        widths: None,
    };
    let loaded = LoadedPlugin {
        inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        latency_samples: Samples::ZERO,
        tail: PluginTail::Unknown,
        features: Features::EDITOR,
        probed: Features::EDITOR,
        ..LoadedPlugin::default()
    };
    let plugin_handle = handle_over(Arc::clone(&bridge), slab, replies, descriptor, loaded);

    (plugin_handle, bridge, bridge_thread, server_thread)
}

#[test]
fn trailing_unsolicited_events_dont_poison_next_reply() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Server scripts `ParameterList` reply *followed by* two unsolicited
    // events (`LatencyChanged`, `ParameterChanged`). Without the dispatch
    // helper, those trailing events would mis-route as the reply to the
    // next `GetParameter` request.
    let event_count = Arc::new(AtomicUsize::new(0));
    let events_for_server = Arc::clone(&event_count);

    let (handle, bridge, _bridge_thread, _server_thread) =
        handle_with_multi_reply_server(move |msg| match msg {
            ControlRequest::GetParameterList => vec![
                Say::from(ControlReply::ParameterList {
                    parameters: vec![ParameterInfo::new(ParamId::new(0), "Vol".to_string())],
                }),
                Say::from(News::LatencyChanged {
                    samples: Samples::new(256),
                }),
                Say::from(News::ParameterChanged {
                    edit: tutti_plugin_types::ParamEdit::change(
                        ParamId::new(7),
                        Normalized::of(0.42),
                    ),
                }),
            ],
            ControlRequest::GetParameter { param_id } if param_id == ParamId::new(0) => {
                vec![Say::from(ControlReply::ParameterValue {
                    value: Some(Normalized::of(0.5)),
                })]
            }
            ControlRequest::GetParameter { .. } => {
                vec![Say::from(ControlReply::ParameterValue { value: None })]
            }
            _ => {
                // Count any other request as a test failure signal.
                events_for_server.fetch_add(1, Ordering::Relaxed);
                vec![]
            }
        });

    let latency_seen = Arc::new(AtomicUsize::new(0));
    let param_seen = Arc::new(parking_lot::Mutex::new(None::<(u32, Normalized)>));

    {
        let latency_seen = Arc::clone(&latency_seen);
        let param_seen = Arc::clone(&param_seen);
        bridge
            .audio()
            .set_listener(Some(Arc::new(move |ev| match ev {
                IpcEvent::LatencyChanged { samples } => {
                    latency_seen.store(samples.get(), Ordering::Release);
                }
                IpcEvent::ParameterChanged { edit } => {
                    *param_seen.lock() = edit.value().map(|v| (edit.id().get(), v));
                }
                IpcEvent::TailChanged { .. }
                | IpcEvent::Resync(_)
                | IpcEvent::IoLayout(_)
                | IpcEvent::RateRefusal(_)
                | IpcEvent::ProcessRequested
                | IpcEvent::Transport { .. }
                | IpcEvent::Crashed { .. } => {}
            })));
    }

    // First request: receives `ParameterList`; the events that follow it are
    // read as they arrive.
    let params = handle.parameter_descriptors().unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name(), "Vol");

    // Second request: the helper must peel the two trailing events off
    // before returning the real `ParameterValue` reply.
    assert_eq!(
        handle.parameter_value(ParamId::new(0)),
        Some(Normalized::of(0.5))
    );

    // The bridge thread fires the listener after the dispatch that drained the
    // unsolicited queue: wait for both events.
    eventually("the listener saw both events", || {
        latency_seen.load(Ordering::Acquire) == 256 && param_seen.lock().is_some()
    });
    assert_eq!(latency_seen.load(Ordering::Acquire), 256);
    assert_eq!(*param_seen.lock(), Some((7, Normalized::of(0.42))));
}

/// A `TailChanged` message arriving on the control stream must reach a
/// listener as a `IpcEvent::TailChanged` carrying the same arm.
///
/// This is the client half of the runtime-tail path. The server half
/// (`a_clap_tail_change_reaches_the_event_list`, in `tutti-plugin-server`)
/// proves the notification becomes a message; this proves the message becomes
/// an event. `PluginClient` itself needs a live subprocess to build, so the
/// final hop — the listener storing into the cell the node's `Shape` tail reads
/// — is exercised by the plugin-server integration suite rather than here.
///
/// `Unbounded` is the arm under test deliberately: it is the one a count
/// cannot carry, so a regression that flattened the enum to a number on the
/// wire would still pass with `Finite`.
#[test]
fn a_tail_change_reaches_the_listener() {
    let (handle, bridge, _bridge_thread, _server_thread) =
        handle_with_multi_reply_server(move |msg| match msg {
            ControlRequest::GetParameterList => vec![
                Say::from(ControlReply::ParameterList {
                    parameters: vec![ParameterInfo::new(ParamId::new(0), "Vol".to_string())],
                }),
                Say::from(News::TailChanged {
                    tail: PluginTail::Unbounded,
                }),
            ],
            ControlRequest::GetParameter { .. } => {
                vec![Say::from(ControlReply::ParameterValue {
                    value: Some(Normalized::of(0.5)),
                })]
            }
            _ => vec![],
        });

    let tail_seen = Arc::new(parking_lot::Mutex::new(None::<PluginTail>));
    {
        let tail_seen = Arc::clone(&tail_seen);
        bridge.audio().set_listener(Some(Arc::new(move |ev| {
            if let IpcEvent::TailChanged { tail } = ev {
                *tail_seen.lock() = Some(tail);
            }
        })));
    }

    // First request takes the reply; the trailing event waits in the socket
    // buffer until the next `recv_reply` drains it.
    assert_eq!(handle.parameter_descriptors().unwrap().len(), 1);
    assert_eq!(
        handle.parameter_value(ParamId::new(0)),
        Some(Normalized::of(0.5))
    );

    eventually("the listener saw the tail change", || {
        tail_seen.lock().is_some()
    });
    assert_eq!(
        *tail_seen.lock(),
        Some(PluginTail::Unbounded),
        "the tail change did not reach the listener as an unbounded tail"
    );
}

#[test]
fn handle_save_state_roundtrip() {
    let (handle, _bridge, _bridge_handle, _server_thread) =
        handle_with_multi_reply_server(|msg| match msg {
            ControlRequest::SaveState { .. } => vec![Say::State(vec![0xDE, 0xAD, 0xBE, 0xEF])],
            _ => Vec::new(),
        });

    let state = handle.save_state(tutti_plugin_types::StateUse::Project);
    assert_eq!(state.expect("mock answered"), vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn handle_get_parameter_list() {
    let (handle, _bridge_handle, _server_thread) = handle_with_mock_server(|msg| match msg {
        ControlRequest::GetParameterList => Some(ControlReply::ParameterList {
            parameters: vec![
                ParameterInfo::new(ParamId::new(0), "Volume".to_string()),
                ParameterInfo::new(ParamId::new(1), "Pan".to_string()),
                ParameterInfo::new(ParamId::new(2), "Cutoff".to_string()),
            ],
        }),
        _ => None,
    });

    let params = handle.parameter_descriptors().unwrap();
    assert_eq!(params.len(), 3);
    assert_eq!(params[0].name(), "Volume");
    assert_eq!(params[1].name(), "Pan");
    assert_eq!(params[2].name(), "Cutoff");
}

#[test]
fn handle_get_parameter_value() {
    let (handle, _bridge_handle, _server_thread) = handle_with_mock_server(|msg| match msg {
        // The mirror's read lists the parameters, then asks for each value.
        ControlRequest::GetParameterList => Some(ControlReply::ParameterList {
            parameters: vec![ParameterInfo::new(ParamId::new(42), "Gain".to_string())],
        }),
        ControlRequest::GetParameter { param_id } if param_id == ParamId::new(42) => {
            Some(ControlReply::ParameterValue {
                value: Some(Normalized::of(0.75)),
            })
        }
        ControlRequest::GetParameter { .. } => Some(ControlReply::ParameterValue { value: None }),
        _ => None,
    });

    assert_eq!(
        handle.parameter_value(ParamId::new(42)),
        Some(Normalized::of(0.75))
    );
}

#[test]
fn handle_open_editor_errors_for_missing_plugin() {
    // open_editor loads the plugin GUI in-process (not via IPC), so with
    // no editor to load it must surface a structured error rather than panic.
    let (handle, _bridge_handle, _server_thread) = handle_with_mock_server(|_| None);
    assert!(
        handle
            .open_editor(
                main_thread(),
                tutti_fixtures::StandInWindow.handle(),
                tutti_plugin_types::ContentScale::IDENTITY
            )
            .is_err()
    );
}
