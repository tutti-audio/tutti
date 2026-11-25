//! A network endpoint as the session's MIDI, over real UDP sockets on loopback. A
//! Host's peer is a bare `Client` on a socket of the test's: each wait is a blocking
//! read under a hang guard, and a Ping is the barrier that says the Host's socket
//! task has taken everything sent before it.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use crate::common::{blocks, offline, tap};
use futures_lite::StreamExt;
use tutti_fixtures::{block_on_within, wait_until};
use tutti_midi_network::{
    Admission, ByeReason, Client, Command, Credentials, DatagramBuilder, EndedBy, EndpointIdentity,
    Event, Host, HostConfig, NetworkEndpoint, NetworkLink, PingId, SessionConfig, parse,
};
use tutti_platform::{Executor, Instant};
use tutti_session::{
    MidiEvent, MidiNetwork, MidiRouteRule, NetworkError, NetworkEvent, OpenClient, OpenHost,
    Session, SessionEvent, Stamp,
};
use tutti_ump::{
    Address, CCNumber, ControlValue, EndpointName, Message, MidiChannel, MidiGroup, NoteNumber,
    NoteVelocity, ProductInstanceId, Sysex7Packet, SysexStatus, U7,
};
use tutti_ump::{Ump, Voice2};

/// A hang guard, never reached by a wait that ends.
const HANG_GUARD: Duration = Duration::from_secs(30);

fn identity(name: &str, id: &str) -> EndpointIdentity {
    EndpointIdentity::new(
        EndpointName::new(name).unwrap(),
        ProductInstanceId::new(id).unwrap(),
    )
    .unwrap()
}

fn note_on(note: u8) -> Ump {
    Message::midi2(
        MidiChannel::FIRST,
        Voice2::note_on(NoteNumber::of(note), NoteVelocity::new(0x8000)),
    )
    .into()
}

/// A Client endpoint on a socket of its own, driven by the test.
struct Peer {
    socket: UdpSocket,
    client: Client,
    host: SocketAddr,
}

impl Peer {
    /// A peer named `name` that will invite the Host listening on `port`.
    fn new(name: &str, port: u16) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(HANG_GUARD)).unwrap();
        let host = SocketAddr::from(([127, 0, 0, 1], port));
        let client = Client::new(
            identity(name, name),
            Credentials::none(),
            SessionConfig::default(),
            host,
            Instant::now(),
        );
        Self {
            socket,
            client,
            host,
        }
    }

    fn flush(&mut self) {
        while let Some(sent) = self.client.poll_transmit() {
            self.socket.send_to(&sent.payload, sent.to).unwrap();
        }
    }

    fn connect(&mut self) {
        self.client.connect(Instant::now());
        self.flush();
    }

    fn recv(&self) -> Vec<u8> {
        let mut buffer = [0u8; 2048];
        let (len, _) = self
            .socket
            .recv_from(&mut buffer)
            .expect("a datagram within the hang guard");
        buffer[..len].to_vec()
    }

    /// Reads datagrams into the Client until it reports an event `want` takes.
    fn next_event<T>(&mut self, mut want: impl FnMut(&Event) -> Option<T>) -> T {
        loop {
            while let Some(event) = self.client.poll_event() {
                if let Some(found) = want(&event) {
                    return found;
                }
            }
            let datagram = self.recv();
            self.client.on_message(self.host, &datagram, Instant::now());
            self.flush();
        }
    }

    /// Reads datagrams until the Client's Session is established.
    fn established(&mut self) {
        self.next_event(|e| matches!(e, Event::Established { .. }).then_some(()));
    }

    /// Returns once the Host's socket task has taken every datagram sent so far: a
    /// Ping is answered after the datagrams before it, and what each of them
    /// queued for the session is queued before the reply is sent.
    fn barrier(&mut self, id: u32) {
        let mut ping = DatagramBuilder::new();
        ping.push(&Command::Ping(PingId::new(id)));
        self.socket.send_to(&ping.finish(), self.host).unwrap();
        loop {
            let datagram = self.recv();
            let replied = parse(&datagram)
                .unwrap()
                .filter_map(Result::ok)
                .any(|c| c == Command::PingReply(PingId::new(id)));
            if replied {
                return;
            }
            self.client.on_message(self.host, &datagram, Instant::now());
            self.flush();
        }
    }

    /// Connects and returns once the Host has the Session, and its socket task has
    /// taken everything sent so far.
    fn join(&mut self) {
        self.connect();
        self.established();
        self.barrier(1);
    }

    /// Reads datagrams until the Session ends: who ended it, and why.
    fn closed(&mut self) -> (EndedBy, ByeReason) {
        self.next_event(|e| match e {
            Event::Closed {
                ended_by, reason, ..
            } => Some((*ended_by, *reason)),
            _ => None,
        })
    }

    /// The UMP the Host has sent so far that the Client has read, once a barrier
    /// has taken the datagrams: in the order they came.
    fn received(&mut self) -> Vec<Ump> {
        std::iter::from_fn(|| self.client.poll_event())
            .filter_map(|event| match event {
                Event::Ump { packet, .. } => Some(packet),
                _ => None,
            })
            .collect()
    }

    fn send(&mut self, packets: &[Ump]) {
        self.client.send(Instant::now(), packets).unwrap();
        self.flush();
    }

    fn close(&mut self, reason: ByeReason) {
        self.client.close(Instant::now(), reason);
        self.flush();
    }
}

/// A session, and a Host endpoint it opened on a port the OS picked.
fn hosting(admission: Admission) -> (Session, MidiNetwork<Host>, u16) {
    let mut s = offline();
    let net = s
        .midi()
        .open_network_host(OpenHost::new(identity("Studio", "HOST-1"), 0).admission(admission))
        .expect("binds");
    let port = net.local_addr().unwrap().port();
    let _ = s.update();
    (s, net, port)
}

/// The network reports of one update.
fn reports(events: Vec<SessionEvent>) -> Vec<NetworkEvent> {
    events
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Midi(MidiEvent::Network(report)) => Some(report),
            _ => None,
        })
        .collect()
}

/// An invitation to a confirming Host is reported and waits; an accept gives the
/// peer an input node in the graph.
#[test]
fn an_invitation_is_reported_and_an_accept_gives_the_peer_an_input_node() {
    let (mut s, net, port) = hosting(Admission::Confirm);
    let mut peer = Peer::new("Laptop", port);
    let laptop = identity("Laptop", "Laptop");
    peer.connect();
    // The Host's Pending reply is sent after the invitation is queued for the session.
    while peer.client.remote().is_none() {
        let datagram = peer.recv();
        peer.client.on_message(peer.host, &datagram, Instant::now());
    }
    let heard = reports(s.update());
    assert!(
        matches!(&heard[..], [NetworkEvent::Invited { identity }] if *identity == laptop),
        "{heard:?}"
    );
    assert_eq!(net.input_node(laptop), None, "an invitation has no input");
    assert_eq!(
        net.accept(identity("Other", "Other")),
        Err(NetworkError::UnknownPeer)
    );

    net.accept(laptop).unwrap();
    peer.established();
    let heard = reports(s.update());
    assert!(
        matches!(&heard[..], [NetworkEvent::Connected { identity }] if *identity == laptop),
        "{heard:?}"
    );
    let node = net.input_node(laptop).expect("the peer has an input node");
    assert_eq!(net.inputs(), [(laptop, node)]);
    assert!(s.graph().contains(node));
    assert_eq!(net.accept(laptop), Err(NetworkError::NoInvitation));
}

/// A decline tells the peer, with its reason, and makes no input entry.
#[test]
fn a_decline_ends_the_invitation_with_its_reason() {
    let (mut s, net, port) = hosting(Admission::Confirm);
    let mut peer = Peer::new("Laptop", port);
    let laptop = identity("Laptop", "Laptop");
    peer.connect();
    peer.barrier(1);
    let _ = s.update();
    net.decline(laptop).unwrap();
    let (by, reason) = peer.closed();
    assert_eq!((by, reason), (EndedBy::Remote, ByeReason::UserDeclined));
    // The peer answered the Bye; the Host's socket task has taken the answer.
    peer.barrier(2);
    let heard = reports(s.update());
    assert!(
        matches!(
            &heard[..],
            [NetworkEvent::Ended { identity, by: EndedBy::Local, reason: ByeReason::UserDeclined }]
                if *identity == laptop
        ),
        "{heard:?}"
    );
    assert!(net.inputs().is_empty());
}

/// UMP a peer sends reaches a node of the graph through the peer's input node and
/// the session's route rule.
#[test]
fn a_peers_ump_reaches_a_graph_node() {
    let (mut s, net, port) = hosting(Admission::Open);
    let (target, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::new().to(target))
        .expect("the tap takes MIDI");
    let mut peer = Peer::new("Laptop", port);
    peer.join();
    let heard = reports(s.update());
    assert!(
        matches!(&heard[..], [NetworkEvent::Connected { .. }]),
        "{heard:?}"
    );

    peer.send(&[note_on(60), note_on(64)]);
    peer.barrier(2);
    let _ = s.update();
    blocks(&mut s, 2);
    let heard: Vec<Ump> = seen.lock().unwrap().iter().map(|(_, ump)| *ump).collect();
    assert_eq!(heard, [note_on(60), note_on(64)], "once and in order");
    drop(net);
}

/// An event the graph sends to the network's output node reaches the peer, as
/// the UMP the spec lays out.
#[test]
fn a_graph_event_reaches_the_peer() {
    let (mut s, net, port) = hosting(Admission::Open);
    let mut keys = s
        .midi()
        .input(net.output_node())
        .expect("the output takes MIDI");
    let _ = s.update();
    let mut peer = Peer::new("Laptop", port);
    peer.join();
    let _ = s.update();

    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(72),
        NoteVelocity::new(0x8000),
    )
    .unwrap();
    blocks(&mut s, 2);
    let _ = s.update();
    let got = peer.next_event(|e| match e {
        Event::Ump { packet, .. } => Some(*packet),
        _ => None,
    });
    assert_eq!(got, note_on(72));
    assert_eq!(net.losses().refused, 0, "a Session took it");
}

/// **A peer that joins an output that has been playing is caught up, and it
/// alone**: the setup the output sent (SysEx, then the channel's program, then
/// its controllers), whatever order it was sent in; the peer already listening
/// hears nothing more. The order is the record's documented catch-up order.
#[test]
fn a_peer_that_joins_is_caught_up_to_what_the_output_said_and_the_others_hear_nothing() {
    let (mut s, net, port) = hosting(Admission::Open);
    let mut keys = s
        .midi()
        .input(net.output_node())
        .expect("the output takes MIDI");
    let _ = s.update();
    let mut a = Peer::new("Laptop", port);
    a.join();
    let _ = s.update();

    let address = Address::first(MidiChannel::FIRST);
    let volume: Ump = Message::midi2(
        address,
        Voice2::ControlChange {
            controller: CCNumber::VOLUME,
            value: ControlValue::new(1 << 30),
        },
    )
    .into();
    let program: Ump = Message::midi2(
        address,
        Voice2::ProgramChange {
            program: U7::of(5),
            bank: None,
        },
    )
    .into();
    let reset: Ump = Message::Sysex7(
        MidiGroup::FIRST,
        Sysex7Packet::new(SysexStatus::Complete, &[0x7F, 0x7F, 0x04, 0x01, 0x00, 0x7F]).unwrap(),
    )
    .into();
    for ump in [volume, program, reset] {
        keys.push(Stamp::NextBlock, ump).unwrap();
    }
    blocks(&mut s, 2);
    let _ = s.update();
    a.barrier(2);
    assert_eq!(
        a.received(),
        [volume, program, reset],
        "heard as it was sent"
    );

    let mut b = Peer::new("Tablet", port);
    b.join();
    let _ = s.update();
    b.barrier(2);
    assert_eq!(
        b.received(),
        [reset, program, volume],
        "SysEx, then the program, then the controllers"
    );
    a.barrier(3);
    assert_eq!(
        a.received(),
        [],
        "a peer already listening hears nothing more"
    );
}

/// With no Session the output has nowhere to send: events are counted refused.
#[test]
fn an_event_with_no_peer_is_counted_refused() {
    let (mut s, net, _) = hosting(Admission::Open);
    let mut keys = s.midi().input(net.output_node()).unwrap();
    let _ = s.update();
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::new(0x8000),
    )
    .unwrap();
    blocks(&mut s, 2);
    let _ = s.update();
    assert_eq!(net.losses().refused, 1);
}

/// A Host's Clients each have an input node of their own; a Bye from one ends its
/// entry alone, reported with who ended it and why.
#[test]
fn a_peers_bye_ends_its_input_entry_alone() {
    let (mut s, net, port) = hosting(Admission::Open);
    let mut stay = Peer::new("Stays", port);
    let mut leave = Peer::new("Leaves", port);
    for peer in [&mut stay, &mut leave] {
        peer.join();
    }
    let heard = reports(s.update());
    assert_eq!(heard.len(), 2, "{heard:?}");
    let (stays, leaves) = (identity("Stays", "Stays"), identity("Leaves", "Leaves"));
    let stays_node = net.input_node(stays).unwrap();
    let leaves_node = net.input_node(leaves).unwrap();
    assert_ne!(stays_node, leaves_node);

    leave.close(ByeReason::UserTerminated);
    leave.next_event(|e| matches!(e, Event::Closed { .. }).then_some(()));
    let heard = reports(s.update());
    assert!(
        matches!(
            &heard[..],
            [NetworkEvent::Ended { identity, by: EndedBy::Remote, reason: ByeReason::UserTerminated }]
                if *identity == leaves
        ),
        "{heard:?}"
    );
    assert_eq!(net.input_node(leaves), None);
    assert_eq!(net.inputs(), [(stays, stays_node)]);
    // The node stays while the graph renders what it releases, then goes.
    blocks(&mut s, 2);
    let _ = s.update();
    assert!(!s.graph().contains(leaves_node));
    assert!(s.graph().contains(stays_node));
}

/// Dropping the handle ends the network: the next update retires the entries and
/// the link's Bye reaches every peer.
#[test]
fn dropping_the_network_sends_bye_to_the_peer() {
    let (mut s, net, port) = hosting(Admission::Open);
    let mut peer = Peer::new("Laptop", port);
    peer.join();
    let _ = s.update();
    let node = net.input_node(identity("Laptop", "Laptop")).unwrap();

    drop(net);
    let _ = s.update();
    let (by, reason) = peer.closed();
    assert_eq!((by, reason), (EndedBy::Remote, ByeReason::PowerDown));
    blocks(&mut s, 2);
    let _ = s.update();
    assert!(!s.graph().contains(node), "the peer's input node left");
}

/// A session ends its links when it drops, whatever handles live: the peer's Bye
/// arrives when the session drops, not when the handle does.
#[test]
fn the_session_ends_its_links_when_it_drops_though_a_handle_lives() {
    let (mut s, net, port) = hosting(Admission::Open);
    let mut peer = Peer::new("Laptop", port);
    peer.join();
    let _ = s.update();
    drop(s);
    assert_eq!(peer.closed(), (EndedBy::Remote, ByeReason::PowerDown));
    drop(net);
}

/// A peer back under its identity from another address replaces its entry, reported
/// before the new one connects, with an input node of its own.
#[test]
fn a_peer_back_from_another_address_replaces_its_entry() {
    let (mut s, net, port) = hosting(Admission::Open);
    let laptop = identity("Laptop", "Laptop");
    let mut first = Peer::new("Laptop", port);
    first.join();
    let _ = s.update();
    let old = net.input_node(laptop).unwrap();

    let mut second = Peer::new("Laptop", port);
    second.join();
    let heard = reports(s.update());
    assert!(
        matches!(
            &heard[..],
            [
                NetworkEvent::Replaced { identity: gone },
                NetworkEvent::Connected { identity: came },
            ] if *gone == laptop && *came == laptop
        ),
        "{heard:?}"
    );
    let new = net.input_node(laptop).unwrap();
    assert_ne!(new, old);
    blocks(&mut s, 2);
    let _ = s.update();
    assert!(!s.graph().contains(old));
    assert!(s.graph().contains(new));
}

/// A session that is a Client of a Host hears it: the Host's UMP reaches the
/// graph through the Host's input node.
#[test]
fn a_client_session_hears_its_host() {
    let executor = Executor::new();
    let host = NetworkLink::bind(
        &executor,
        "127.0.0.1:0",
        Host::new(HostConfig::new(identity("Studio", "HOST-1"))),
    )
    .unwrap();
    let addr = host.local_addr().unwrap();
    let mut s = offline();
    let (target, seen) = tap(&mut s);
    s.midi().route(MidiRouteRule::new().to(target)).unwrap();
    let net = s
        .midi()
        .open_network_client(OpenClient::new(
            identity("Laptop", "CLIENT-1"),
            SocketAddr::from(([127, 0, 0, 1], addr.port())),
        ))
        .expect("binds");

    // The Host's link has the Session established once its stream says so.
    block_on_within(
        host.events()
            .find(|event| matches!(event, Event::Established { .. })),
        HANG_GUARD,
    )
    .expect("the Host establishes within the hang guard");
    // The session learns of its end of it in `update`, which is the session's only
    // read of its link, so the wait is a look at `update` until the link's task,
    // which another thread runs, has queued it.
    let studio = identity("Studio", "HOST-1");
    let mut heard = Vec::new();
    wait_until("the session to connect", HANG_GUARD, || {
        heard.extend(reports(s.update()));
        net.input_node(studio).is_some()
    });
    assert!(matches!(
        heard.first(),
        Some(NetworkEvent::Connected { identity }) if *identity == studio
    ));
    assert_eq!(host.with(|h, _| h.session_count()), 1);
    let client = SocketAddr::from(([127, 0, 0, 1], net.local_addr().unwrap().port()));
    host.with(|h, now| h.send(now, client, &[note_on(65)]))
        .unwrap();
    wait_until("the Host's UMP to reach the graph", HANG_GUARD, || {
        let _ = s.update();
        blocks(&mut s, 1);
        !seen.lock().unwrap().is_empty()
    });
    assert_eq!(seen.lock().unwrap()[0].1, note_on(65));
}
