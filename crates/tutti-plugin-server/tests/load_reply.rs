//! A failed load is answered, and the server stays up.
//!
//! Drives the real `plugin-server` binary over its control socket: a file that
//! is not a plugin gets an `Error` reply carrying the loader's reason, and the
//! same server then loads a good plugin. A server that exited on the failed
//! load (the host reports that as `ProcessCrashed`) fails the second step.
//!
//! The reference CLAP plugin loads under its library's own name, which no
//! format claims by extension: the load names the format.

#![cfg(all(unix, feature = "clap", feature = "vst3"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use interprocess::local_socket::Stream;
use interprocess::local_socket::traits::Stream as _;
use tutti_buffers::FloatWidth;
use tutti_plugin_ipc::{
    ControlFrame, ControlReply, ControlRequest, Lane, LaneWire, ReqId, Request, Working,
    read_frame, socket_name, write_frame,
};
use tutti_plugin_types::{PluginFormat, PluginId, SampleRate, ServerError};

/// Kills the server if the test fails before it shuts it down.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect(socket: &Path, server: &mut Server) -> Stream {
    let mut connected = None;
    tutti_fixtures::wait_until(
        "the server to bind its socket",
        Duration::from_secs(60),
        || {
            if let Ok(stream) = Stream::connect(socket_name(socket).expect("a socket name")) {
                connected = Some(stream);
                return true;
            }
            assert!(
                server.0.try_wait().expect("poll the server").is_none(),
                "the server exited before it bound its socket"
            );
            false
        },
    );
    connected.expect("the wait ended on a connection")
}

/// Asks for the audio lane over `broker` as the host does: the `OpenLane` frame
/// of request `id`, then the server's end of a fresh socket pair as a descriptor.
/// Returns the host's end and the server's answer.
fn request_audio_lane(broker: &mut Stream, id: u64) -> (Stream, ControlReply) {
    use std::os::fd::AsFd as _;
    let (host, server) = tutti_plugin_ipc_ffi::lane_pair().expect("a socket pair");
    write_frame(
        broker,
        &Request {
            id: ReqId::new(id),
            body: ControlRequest::OpenLane {
                lane: Lane::Audio,
                wire: LaneWire::fresh().expect("a lane wire"),
            },
        },
    )
    .expect("send OpenLane");
    let Stream::UdSocket(socket) = &*broker;
    tutti_plugin_ipc_ffi::send_lane(socket.as_fd(), server.as_fd()).expect("send the lane's end");
    match read_frame(broker).expect("the server answers the lane") {
        ControlFrame::Reply { id: of, reply } => {
            assert_eq!(of, ReqId::new(id), "the answer echoes the request's id");
            (Stream::UdSocket(host.into()), reply)
        }
        other => panic!("expected the answer to the lane, got {other:?}"),
    }
}

/// [`request_audio_lane`], which the server opens: the host's end of the lane.
fn open_audio_lane(broker: &mut Stream) -> Stream {
    match request_audio_lane(broker, 2) {
        (lane, ControlReply::LaneOpened) => lane,
        (_, other) => panic!("expected LaneOpened, got {other:?}"),
    }
}

fn load(stream: &mut Stream, path: &Path, format: PluginFormat) -> ControlReply {
    load_choosing(stream, path, format, None)
}

fn load_choosing(
    stream: &mut Stream,
    path: &Path,
    format: PluginFormat,
    plugin: Option<PluginId>,
) -> ControlReply {
    write_frame(
        stream,
        &Request {
            id: ReqId::new(1),
            body: ControlRequest::LoadPlugin {
                path: path.to_path_buf(),
                format,
                plugin,
                sample_rate: SampleRate::SR_48K,
                block_size: 512,
                preferred_format: FloatWidth::F32,
                state: None,
            },
        },
    )
    .expect("send LoadPlugin");
    // The server says it is working on a load before it answers.
    loop {
        match read_frame(stream).expect("the server answers a load; it did not exit") {
            ControlFrame::Reply { id, reply } => {
                assert_eq!(id, ReqId::new(1), "the answer echoes the request's id");
                return reply;
            }
            ControlFrame::Working { id, .. } => assert_eq!(id, ReqId::new(1)),
            other => panic!("expected the answer to the load, got {other:?}"),
        }
    }
}

/// The reference CLAP plugin's library, and a copy of it named `.vst3` that
/// the VST3 format refuses because the library is not VST3.
fn fixtures() -> (PathBuf, PathBuf) {
    let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
    let not_a_plugin = tutti_fixtures::publish_with_extension(&real, "vst3");
    (not_a_plugin, real)
}

/// A `plugin-server` process and its broker connection, past `Hello`.
fn start_server() -> (Server, Stream) {
    let (server, stream, _) = start_server_for_this_process();
    (server, stream)
}

/// [`start_server`], and the socket it serves. The server is this process's
/// child and takes this process for its host.
fn start_server_for_this_process() -> (Server, Stream, PathBuf) {
    // A fresh socket path that fits a socket address.
    let socket = tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source");

    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_plugin-server"))
            .arg(&socket)
            .env(
                tutti_plugin_ipc_ffi::HOST_PID_ENV,
                std::process::id().to_string(),
            )
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn plugin-server"),
    );
    let mut stream = connect(&socket, &mut server);
    assert!(matches!(
        read_frame::<_, ControlFrame>(&mut stream),
        Ok(ControlFrame::Hello { .. })
    ));
    (server, stream, socket)
}

/// **A load is announced as work in progress before it is answered**, under the
/// request's own id: the host's wait for it restarts on that frame, so a plugin
/// that opens slowly is waited for, and the answer echoes the same id.
#[test]
fn a_load_is_announced_as_working_before_it_is_answered() {
    let (_not_a_plugin, good) = fixtures();
    let (server, mut stream) = start_server();
    write_frame(
        &mut stream,
        &Request {
            id: ReqId::new(41),
            body: ControlRequest::LoadPlugin {
                path: good,
                format: PluginFormat::Clap,
                plugin: None,
                sample_rate: SampleRate::SR_48K,
                block_size: 512,
                preferred_format: FloatWidth::F32,
                state: None,
            },
        },
    )
    .expect("send LoadPlugin");
    match read_frame::<_, ControlFrame>(&mut stream).expect("the server speaks") {
        ControlFrame::Working { id, stage } => {
            assert_eq!(id, ReqId::new(41));
            assert_eq!(stage, Working::Opening);
        }
        other => panic!("the first frame of a load is a sign of life, got {other:?}"),
    }
    loop {
        match read_frame::<_, ControlFrame>(&mut stream).expect("the load is answered") {
            ControlFrame::Working { id, .. } => assert_eq!(id, ReqId::new(41)),
            ControlFrame::Reply { id, reply } => {
                assert_eq!(id, ReqId::new(41));
                assert!(
                    matches!(reply, ControlReply::PluginLoaded { .. }),
                    "{reply:?}"
                );
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    drop(server);
}

/// A load that fails gets the loader's reason and leaves the server serving.
#[test]
fn a_failed_load_is_answered_and_the_server_still_loads_a_good_plugin() {
    let (not_a_plugin, good) = fixtures();
    let (mut server, mut stream) = start_server();

    match load(&mut stream, &not_a_plugin, PluginFormat::Vst3) {
        ControlReply::Error {
            error: ServerError::Load { stage, reason },
        } => {
            assert!(
                !reason.contains("unsupported plugin format"),
                "the library was refused before a format read it: {reason}"
            );
            assert!(
                reason.contains("no GetPluginFactory symbol"),
                "the reply names the format's reason at {stage}: {reason}"
            );
        }
        other => panic!("expected an Error reply, got {other:?}"),
    }
    assert!(
        server.0.try_wait().expect("poll the server").is_none(),
        "the server stays alive after a failed load"
    );

    assert!(
        matches!(
            load(&mut stream, &good, PluginFormat::Clap),
            ControlReply::PluginLoaded { .. }
        ),
        "the same server loads a good plugin after a failed one"
    );

    write_frame(
        &mut stream,
        &Request {
            id: ReqId::new(2),
            body: ControlRequest::Shutdown,
        },
    )
    .expect("send Shutdown");
    drop(server);
}

/// **A load that chooses a plugin of a file that exports several loads that
/// one, through the real `plugin-server`** (the choice crosses the wire in
/// `LoadPlugin::plugin`). The reference CLAP file is a factory of
/// two: the first without a choice, the second when named.
#[test]
fn a_load_that_names_a_plugin_loads_that_plugin_in_the_server() {
    let (_not_a_plugin, good) = fixtures();
    let (server, mut stream) = start_server();
    let id_of = |reply: ControlReply| match reply {
        ControlReply::PluginLoaded { id, .. } => id,
        other => panic!("expected PluginLoaded, got {other:?}"),
    };
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID
        .to_str()
        .expect("utf-8");

    assert_eq!(
        id_of(load(&mut stream, &good, PluginFormat::Clap)),
        PluginId::Clap(
            tutti_clap_test_plugin::PLUGIN_ID
                .to_str()
                .expect("utf-8")
                .into()
        ),
        "no choice loads the first plugin"
    );
    assert_eq!(
        id_of(load_choosing(
            &mut stream,
            &good,
            PluginFormat::Clap,
            Some(PluginId::Clap(second.into()))
        )),
        PluginId::Clap(second.into()),
        "the named plugin loads, not the first"
    );

    write_frame(
        &mut stream,
        &Request {
            id: ReqId::new(2),
            body: ControlRequest::Shutdown,
        },
    )
    .expect("send Shutdown");
    drop(server);
}

/// Waits for the server to end and returns its status and what it logged (to
/// standard error).
fn ended(server: &mut Server) -> (std::process::ExitStatus, String) {
    let mut status = None;
    tutti_fixtures::wait_until("the server to end", Duration::from_secs(60), || {
        status = server.0.try_wait().expect("poll the server");
        status.is_some()
    });
    let mut printed = String::new();
    std::io::Read::read_to_string(&mut server.0.stderr.take().expect("piped"), &mut printed)
        .expect("what the server logged");
    (status.expect("the wait ended on a status"), printed)
}

/// **A handshake that ends in `Shutdown` releases the plugin it loaded.** The
/// plugin's Processor is lent at its activation to a seat that only the render
/// thread polls, and no render thread exists before the audio phase: an exit
/// that drops the plugin there leaves it leaked, with a log that blames a
/// render thread that was never started.
#[test]
fn a_shutdown_in_the_handshake_releases_the_plugin() {
    let (_not_a_plugin, good) = fixtures();
    let (mut server, mut stream) = start_server();
    assert!(matches!(
        load(&mut stream, &good, PluginFormat::Clap),
        ControlReply::PluginLoaded { .. }
    ));

    write_frame(
        &mut stream,
        &Request {
            id: ReqId::new(2),
            body: ControlRequest::Shutdown,
        },
    )
    .expect("send Shutdown");
    let (status, logged) = ended(&mut server);
    // Not 3 (`EXIT_PLUGIN_LEAKED`): the plugin was destroyed, not leaked.
    assert_eq!(status.code(), Some(0), "{status:?}: {logged}");
}

/// **A second lane is refused by name, and the first goes on**: the server
/// answers the second `OpenLane` with `LaneAlreadyOpen`, stays up, and ends clean
/// at the host's `Shutdown`.
#[test]
fn a_second_lane_is_refused_by_name_and_the_session_goes_on() {
    let (_not_a_plugin, good) = fixtures();
    let (mut server, mut stream) = start_server();
    assert!(matches!(
        load(&mut stream, &good, PluginFormat::Clap),
        ControlReply::PluginLoaded { .. }
    ));
    let _audio = open_audio_lane(&mut stream);

    let (_second, reply) = request_audio_lane(&mut stream, 3);
    assert!(
        matches!(
            reply,
            ControlReply::Error {
                error: ServerError::LaneAlreadyOpen
            }
        ),
        "{reply:?}"
    );
    write_frame(
        &mut stream,
        &Request {
            id: ReqId::new(4),
            body: ControlRequest::Shutdown,
        },
    )
    .expect("send Shutdown");
    let (status, logged) = ended(&mut server);
    assert_eq!(status.code(), Some(0), "{status:?}: {logged}");
}

/// **A handshake frame that does not decode ends the server, with that cause
/// and not as a host that went away.** A host of another protocol sends a
/// well-framed body the server cannot read; the server used to take it for the
/// handshake's end and wait for an audio connection from that peer.
#[test]
fn a_handshake_frame_that_does_not_decode_ends_the_server() {
    use std::io::Write as _;

    let (mut server, mut stream) = start_server();
    // A length prefix of four, and four bytes no message begins with.
    stream
        .write_all(&[0, 0, 0, 4, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("send a frame");
    let (status, _) = ended(&mut server);
    assert!(!status.success(), "the server ended as if the host left");
}

/// **A host that ends once the session is running shuts the server down as an
/// end of the control connection does, and the plugin is released.** This test
/// process keeps the connections open, so no end of stream says the host is gone:
/// only the watch on the host process does. The host is a shell that states its
/// pid and spawns the server. When it ends, the server's own teardown closes its
/// sockets (the stream ends) and, once the plugin has gone home and the render
/// thread has stopped, logs that its teardown is done, which an end by `SIGKILL`
/// cannot do; the log carries no plugin it had to leak.
#[test]
fn a_host_that_ends_in_the_control_phase_shuts_the_server_down() {
    use std::io::BufRead as _;

    let (_not_a_plugin, good) = fixtures();
    let socket = tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source");
    let dir = tempfile::tempdir().expect("a scratch directory");
    let log = dir.path().join("server.log");
    // The shell is the host until its input closes: it states its own pid, prints
    // the server's, and waits.
    let script = format!(
        "RUST_LOG=info {env}=$$ {exe:?} {socket:?} >{log:?} 2>&1 & echo $!; read _",
        env = tutti_plugin_ipc_ffi::HOST_PID_ENV,
        exe = env!("CARGO_BIN_EXE_plugin-server"),
    );
    let mut host = Server(
        Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the host shell"),
    );
    let mut line = String::new();
    std::io::BufReader::new(host.0.stdout.take().expect("piped"))
        .read_line(&mut line)
        .expect("the shell prints the server's pid");
    let server: u32 = line.trim().parse().expect("the server's pid");

    let mut handshake = connect(&socket, &mut host);
    assert!(matches!(
        read_frame::<_, ControlFrame>(&mut handshake),
        Ok(ControlFrame::Hello { .. })
    ));
    assert!(matches!(
        load(&mut handshake, &good, PluginFormat::Clap),
        ControlReply::PluginLoaded { .. }
    ));
    let _audio = open_audio_lane(&mut handshake);
    let mut control = handshake;
    let (ended, server_ended) = std::sync::mpsc::channel();
    let executor = tutti_platform::Executor::new();
    let _watch = tutti_plugin_ipc_ffi::watch_host(&executor, server, move || {
        ended.send(()).expect("listening");
    })
    .expect("the watch starts");

    drop(host.0.stdin.take());
    host.0.wait().expect("the host ends");
    // The server's teardown closes its end of the control connection.
    assert!(
        read_frame::<_, ControlFrame>(&mut control).is_err(),
        "the server answered after its host ended"
    );
    server_ended.recv().expect("the server ends");

    let printed = std::fs::read_to_string(&log).expect("the server's log");
    assert!(
        printed.contains("the control phase ended cleanly and its teardown is done"),
        "no clean teardown in the log: {printed}"
    );
    assert!(
        !printed.contains("leaking the plugin"),
        "the plugin was leaked: {printed}"
    );
}
