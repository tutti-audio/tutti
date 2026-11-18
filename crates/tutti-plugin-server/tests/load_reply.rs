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
use tutti_plugin_ipc::{BridgeMessage, HostMessage, read_frame, socket_name, write_frame};
use tutti_plugin_types::SampleFormat;
use tutti_plugin_types::{Load, PluginFormat, SampleRate};

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

fn load(stream: &mut Stream, path: &Path, format: PluginFormat) -> BridgeMessage {
    load_choosing(stream, path, format, Load::new())
}

fn load_choosing(
    stream: &mut Stream,
    path: &Path,
    format: PluginFormat,
    load: Load,
) -> BridgeMessage {
    write_frame(
        stream,
        &HostMessage::LoadPlugin {
            path: path.to_path_buf(),
            format,
            load,
            sample_rate: SampleRate::SR_48K,
            block_size: 512,
            preferred_format: SampleFormat::Float32,
            shm_name: "load-reply".into(),
            state: None,
        },
    )
    .expect("send LoadPlugin");
    read_frame(stream).expect("the server answers a load; it did not exit")
}

/// The reference CLAP plugin's library, and a copy of it named `.vst3` that
/// the VST3 format refuses because the library is not VST3.
fn fixtures() -> (PathBuf, PathBuf) {
    let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
    let not_a_plugin = tutti_fixtures::publish_with_extension(&real, "vst3");
    (not_a_plugin, real)
}

/// A `plugin-server` process and its handshake connection, past `Ready`.
fn start_server() -> (Server, Stream) {
    // The default config makes a fresh socket path that fits a socket address.
    let socket = tutti_plugin_ipc::BridgeConfig::at(
        tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source"),
    )
    .socket_path;

    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_plugin-server"))
            .arg(&socket)
            .env("TUTTI_PLUGIN_HOST_PID", std::process::id().to_string())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn plugin-server"),
    );
    let mut stream = connect(&socket, &mut server);
    assert!(matches!(
        read_frame::<_, BridgeMessage>(&mut stream),
        Ok(BridgeMessage::Ready { .. })
    ));
    (server, stream)
}

/// A load that fails gets the loader's reason and leaves the server serving.
#[test]
fn a_failed_load_is_answered_and_the_server_still_loads_a_good_plugin() {
    let (not_a_plugin, good) = fixtures();
    let (mut server, mut stream) = start_server();

    match load(&mut stream, &not_a_plugin, PluginFormat::Vst3) {
        BridgeMessage::Error { message } => {
            assert!(
                !message.contains("Unsupported plugin format"),
                "the library was refused before a format read it: {message}"
            );
            assert!(
                message.starts_with("Opening:") || message.contains("ailed"),
                "the reply names the format's reason: {message}"
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
            BridgeMessage::PluginLoaded { .. }
        ),
        "the same server loads a good plugin after a failed one"
    );

    write_frame(&mut stream, &HostMessage::Shutdown).expect("send Shutdown");
    drop(server);
}

/// **A load that chooses a plugin of a file that exports several loads that
/// one, through the real `plugin-server`** (the choice crosses the wire in
/// `LoadPlugin::load`, protocol 28). The reference CLAP file is a factory of
/// two: the first without a choice, the second when named.
#[test]
fn a_load_that_names_a_plugin_loads_that_plugin_in_the_server() {
    let (_not_a_plugin, good) = fixtures();
    let (server, mut stream) = start_server();
    let id_of = |reply: BridgeMessage| match reply {
        BridgeMessage::PluginLoaded { descriptor, .. } => descriptor.id,
        other => panic!("expected PluginLoaded, got {other:?}"),
    };
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID
        .to_str()
        .expect("utf-8");

    assert_eq!(
        id_of(load(&mut stream, &good, PluginFormat::Clap)),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8"),
        "no choice loads the first plugin"
    );
    assert_eq!(
        id_of(load_choosing(
            &mut stream,
            &good,
            PluginFormat::Clap,
            Load::new().plugin(second)
        )),
        second,
        "the named plugin loads, not the first"
    );

    write_frame(&mut stream, &HostMessage::Shutdown).expect("send Shutdown");
    drop(server);
}

/// Waits for the server to end and returns its status and what it printed.
fn ended(server: &mut Server) -> (std::process::ExitStatus, String) {
    let mut status = None;
    tutti_fixtures::wait_until("the server to end", Duration::from_secs(60), || {
        status = server.0.try_wait().expect("poll the server");
        status.is_some()
    });
    let mut printed = String::new();
    std::io::Read::read_to_string(&mut server.0.stdout.take().expect("piped"), &mut printed)
        .expect("what the server printed");
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
        BridgeMessage::PluginLoaded { .. }
    ));

    write_frame(&mut stream, &HostMessage::Shutdown).expect("send Shutdown");
    let (status, printed) = ended(&mut server);
    assert!(status.success(), "{status:?}");
    assert!(
        !printed.contains("leaking the plugin"),
        "the plugin was leaked: {printed}"
    );
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
