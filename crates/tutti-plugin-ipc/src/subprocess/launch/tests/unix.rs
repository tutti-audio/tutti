//! The launch's unix tests: the audio lane's open ([`open_audio_lane`]) against a
//! mock server on the other end of a socket pair (the lane's end travels as a
//! descriptor), and what a spawned server inherits.

use super::*;
use crate::transport::broker_pair;
use crate::{ControlRequest, Request};

/// The host's `Launch` over the host end of a broker pair, and the server's end.
fn broker(t: &Timeouts) -> (Launch<'_>, ControlStream) {
    let (host, server) = broker_pair();
    (Launch::new(host, t), server)
}

/// A mock server: reads the `OpenLane` frame and the lane's end that follows it.
fn take_open_lane(server: &mut ControlStream) -> (ReqId, std::os::fd::OwnedFd) {
    use std::os::fd::AsFd as _;
    let Request { id, body } =
        ipc::recv_within(server, Duration::from_secs(60)).expect("the OpenLane frame arrives");
    let ControlRequest::OpenLane { lane, wire } = body else {
        panic!("expected OpenLane, got {body:?}");
    };
    assert_eq!(lane, Lane::Audio);
    let interprocess::local_socket::Stream::UdSocket(socket) = &*server;
    let end = tutti_plugin_ipc_ffi::receive_lane(socket.as_fd(), &wire)
        .expect("the lane's end arrives after its frame");
    (id, end)
}

/// **The lane `open_audio_lane` returns is the server's end's peer**: the
/// descriptor the server received is a live duplex stream to the host's, and
/// each side's frame parses on the other.
#[test]
fn an_opened_lane_is_a_live_duplex_stream_to_the_server() {
    let t = Timeouts::default();
    let (mut launch, mut server) = broker(&t);
    let config = IpcConfig::new();
    let mock = std::thread::spawn(move || {
        let (id, end) = take_open_lane(&mut server);
        let mut lane = interprocess::local_socket::Stream::UdSocket(end.into());
        ipc::send(
            &mut server,
            &ControlFrame::Reply {
                id,
                reply: ControlReply::LaneOpened,
            },
        )
        .expect("LaneOpened");
        let got: crate::AudioRequest =
            ipc::recv_within(&mut lane, Duration::from_secs(60)).expect("the host's block");
        assert!(matches!(got, crate::AudioRequest::Reset));
        ipc::send(
            &mut lane,
            &crate::AudioReply::ParametersApplied { id: ReqId::new(5) },
        )
        .expect("a reply");
        lane
    });
    let mut audio = open_audio_lane(&mut launch, &config).expect("the lane opens");
    ipc::send(&mut audio, &crate::AudioRequest::Reset).expect("a request");
    let reply: crate::AudioReply =
        ipc::recv_within(&mut audio, Duration::from_secs(60)).expect("the server's reply");
    assert!(matches!(
        reply,
        crate::AudioReply::ParametersApplied { id } if id == ReqId::new(5)
    ));
    let _lane = mock.join().expect("the mock server ends");
}

/// **A server that ends while the lane opens is a crashed process**: the
/// broker closes after the `OpenLane` frame and its end were read, and the host
/// hears the end of the stream, not a timeout or a lane.
#[test]
fn a_server_that_ends_mid_open_lane_is_a_process_crash() {
    let t = Timeouts::default();
    let (mut launch, mut server) = broker(&t);
    let config = IpcConfig::new();
    let mock = std::thread::spawn(move || {
        let _ = take_open_lane(&mut server);
    });
    let err = open_audio_lane(&mut launch, &config).expect_err("no lane");
    assert!(matches!(err, IpcError::ProcessCrashed), "{err}");
    mock.join().expect("the mock server ends");
}

/// **A server answers the lane with `LaneOpened` or the open fails**: an
/// error reply is refused as the unexpected answer it is.
#[test]
fn a_lane_answered_with_another_reply_is_refused() {
    let t = Timeouts::default();
    let (mut launch, mut server) = broker(&t);
    let config = IpcConfig::new();
    let mock = std::thread::spawn(move || {
        let (id, _end) = take_open_lane(&mut server);
        ipc::send(
            &mut server,
            &ControlFrame::Reply {
                id,
                reply: ControlReply::Error {
                    error: ServerError::LaneAlreadyOpen,
                },
            },
        )
        .expect("the refusal");
        server
    });
    let err = open_audio_lane(&mut launch, &config).expect_err("no lane");
    assert!(
        matches!(
            err,
            IpcError::UnexpectedMessage {
                expected: "LaneOpened",
                ..
            }
        ),
        "{err}"
    );
    let _server = mock.join().expect("the mock server ends");
}

/// Set only in the child of [`stand_in_report`], so that [`stand_in_reports`]
/// reports there and is an instant no-op in an ordinary test run.
const STAND_IN_ENV: &str = "TUTTI_LAUNCH_TEST_STAND_IN";

/// What a server spawned by [`crate::transport::spawn`] sees of its
/// inheritance, as its own report, while this process holds a descriptor open
/// without close-on-exec: the lines `fd N`, `arg A` and `env K`, then `byte B`
/// for the first byte the host wrote to its standard input.
///
/// The server is **this test binary**, re-run on [`stand_in_reports`] (a program
/// looked up on `PATH` is not one on every runner), under the same
/// `spawn` a launch uses, and the report is written to a fifo whose path is
/// the value of [`STAND_IN_ENV`] (the server's standard output is the host's).
fn stand_in_report() -> (Vec<String>, Vec<String>) {
    use std::io::{Read as _, Write as _};

    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, rest)| rest);
    let args = vec![
        "--exact".to_owned(),
        format!("{module}::stand_in_reports"),
        "--nocapture".to_owned(),
    ];
    // A descriptor this process holds without close-on-exec, which a bare
    // spawn would hand on: the spawn's sweep keeps it from the server.
    let leaked = std::fs::File::open("/dev/null").expect("/dev/null opens");
    rustix::io::fcntl_setfd(&leaked, rustix::io::FdFlags::empty()).expect("close-on-exec clears");
    let mut command = Command::new(std::env::current_exe().expect("the test binary's path"));
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tutti-launch-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("a directory");
    let fifo = dir.join("report");
    let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
    assert!(made.success(), "mkfifo {}", fifo.display());
    command.args(&args).env(STAND_IN_ENV, &fifo);
    let (server, rendezvous) = crate::transport::spawn(command).expect("the stand-in spawns");
    let guard = ProcessGuard::launched(server, &IpcConfig::new(), tutti_fixtures::executor())
        .expect("the exit thread starts");
    let mut host = rendezvous
        .connect(&guard, Duration::from_secs(60))
        .expect("the host's end exists from the spawn");
    host.write_all(&[0x5a])
        .expect("the byte reaches the stand-in");
    // The stand-in opens the fifo after it listed its descriptors; the read
    // ends at its end.
    let mut report = String::new();
    std::fs::File::open(&fifo)
        .expect("the fifo opens")
        .read_to_string(&mut report)
        .expect("the report");
    std::fs::remove_dir_all(&dir).expect("the directory goes");
    drop(guard);
    drop(leaked);
    let lines = report
        .lines()
        .filter_map(|l| l.strip_prefix("report: "))
        .map(str::to_owned)
        .collect();
    (args, lines)
}

/// The body of the stand-in [`stand_in_report`] spawns; a no-op anywhere else.
#[test]
fn stand_in_reports() {
    use std::fmt::Write as _;
    use std::io::Read as _;

    let Some(fifo) = std::env::var_os(STAND_IN_ENV) else {
        return;
    };
    let mut out = String::new();
    let mut byte = [0u8; 1];
    std::io::stdin()
        .read_exact(&mut byte)
        .expect("the host's byte");
    writeln!(out, "report: byte {}", byte[0]).expect("a line");
    // The directory's own descriptor is listed while it is read and gone when
    // that ends: only a descriptor `metadata` still finds is the process's.
    let numbers: Vec<String> = std::fs::read_dir("/dev/fd")
        .expect("the descriptor table")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    for n in numbers {
        if std::fs::metadata(format!("/dev/fd/{n}")).is_ok() {
            writeln!(out, "report: fd {n}").expect("a line");
        }
    }
    for arg in std::env::args().skip(1) {
        writeln!(out, "report: arg {arg}").expect("a line");
    }
    for (key, _) in std::env::vars_os() {
        writeln!(out, "report: env {}", key.to_string_lossy()).expect("a line");
    }
    std::fs::write(fifo, out).expect("the report");
}

/// **The server holds descriptors 0, 1 and 2 and nothing else**, even with one
/// held open without close-on-exec in the host (and whatever its runner
/// leaked), and the byte the host wrote arrives on 0. The host's
/// end of the pair, a slab or a lane among them would let the server, and the
/// plugin in it, hold what is not its.
#[test]
fn the_spawn_adds_no_descriptor_but_stdin() {
    let (_, report) = stand_in_report();
    let mut fds: Vec<u32> = report
        .iter()
        .filter_map(|l| l.strip_prefix("fd "))
        .map(|n| n.parse().expect("a descriptor number"))
        .collect();
    fds.sort_unstable();
    assert_eq!(fds, [0, 1, 2], "{report:?}");
    assert!(
        report.contains(&"byte 90".to_owned()),
        "the host's byte arrives on stdin: {report:?}"
    );
}

/// **Nothing names the connection in the server's command line or
/// environment**: its arguments are exactly the ones the command was given, and
/// the spawn added no variable (the launch adds only the host's pid, for the
/// server's watch).
#[test]
fn argv_and_env_name_no_endpoint() {
    let (args, report) = stand_in_report();
    let seen = |kind: &str| -> Vec<String> {
        report
            .iter()
            .filter_map(|l| l.strip_prefix(kind))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(seen("arg "), args);
    let mut env = seen("env ");
    env.sort();
    let mut expected: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .chain([STAND_IN_ENV.to_owned()])
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(env, expected);
}
