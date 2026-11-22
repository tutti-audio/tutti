//! The audio lane's open ([`open_audio_lane`]), against a mock server on the other
//! end of a socket pair: the lane's end travels as a descriptor, so these tests
//! are unix's.

use super::*;
use crate::transport::broker_pair;
use std::path::PathBuf;

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
    let config = IpcConfig::at(PathBuf::new());
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
    let config = IpcConfig::at(PathBuf::new());
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
    let config = IpcConfig::at(PathBuf::new());
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
