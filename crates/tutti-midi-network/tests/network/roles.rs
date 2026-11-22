//! Several Clients on one Host (M2-124 section 8) and one device in both
//! roles (section 9), over the simulated network.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use tutti_platform::Instant;

use tutti_midi_network::{
    Admission, AuthMethods, Bye, ByeReason, Client, Command, Credentials, DatagramBuilder, Digest,
    Event, Host, HostConfig, Invitation, NetworkEndpoint, PingId, SessionConfig, SessionState,
    Username, identity_from_txt, parse, txt_properties,
};

use crate::sim::{host_addr, identity};

/// A Client and the address it sends from.
struct Peer {
    addr: SocketAddr,
    client: Client,
}

fn peer(port: u16, name: &str, id: &str, now: Instant) -> Peer {
    Peer {
        addr: SocketAddr::from(([10, 0, 0, 9], port)),
        client: Client::new(
            identity(name, id),
            Credentials::none(),
            SessionConfig::default(),
            host_addr(),
            now,
        ),
    }
}

/// Moves datagrams between the Host and its Clients until none is queued.
fn pump(host: &mut Host, peers: &mut [Peer], now: Instant) {
    loop {
        let mut moved = false;
        for p in peers.iter_mut() {
            while let Some(t) = p.client.poll_transmit() {
                moved = true;
                host.on_message(p.addr, &t.payload, now);
            }
        }
        while let Some(t) = host.poll_transmit() {
            moved = true;
            if let Some(p) = peers.iter_mut().find(|p| p.addr == t.to) {
                p.client.on_message(host_addr(), &t.payload, now);
            }
        }
        if !moved {
            break;
        }
    }
}

fn host_taking(sessions: usize) -> Host {
    let mut config = HostConfig::new(identity("Studio", "HOST-1"));
    config.max_sessions = NonZeroUsize::new(sessions).unwrap();
    Host::new(config)
}

fn datagram(command: &Command) -> Vec<u8> {
    let mut b = DatagramBuilder::new();
    b.push(command);
    b.finish()
}

fn commands(bytes: &[u8]) -> Vec<Command> {
    parse(bytes).unwrap().map(Result::unwrap).collect()
}

/// Spec: m2-124/8#1, m2-124/3.2#4
#[test]
fn several_clients_hold_sessions_with_one_host_each_known_by_its_address() {
    let now = Instant::now();
    let mut host = host_taking(16);
    let mut peers: Vec<Peer> = (0..3)
        .map(|i| {
            let tag = format!("C-{i}");
            peer(50_000 + i, &tag, &tag, now)
        })
        .collect();
    for p in &mut peers {
        p.client.connect(now);
    }
    pump(&mut host, &mut peers, now);
    assert!(
        peers
            .iter()
            .all(|p| p.client.state() == SessionState::Established)
    );
    assert_eq!(host.session_count(), 3);
    // Data to one Client reaches that Client alone.
    let note = crate::sim::note_on(60);
    host.send(now, peers[1].addr, &[note]).unwrap();
    pump(&mut host, &mut peers, now);
    let mut heard = Vec::new();
    for p in &mut peers {
        let mut umps = 0;
        while let Some(e) = p.client.poll_event() {
            umps += usize::from(matches!(e, Event::Ump { .. }));
        }
        heard.push(umps);
    }
    assert_eq!(heard, [0, 1, 0]);
}

/// Spec: m2-124/8#2, m2-124/8#3
#[test]
fn a_host_with_a_session_limit_refuses_the_client_past_it_until_a_session_ends() {
    let now = Instant::now();
    let mut host = host_taking(2);
    let mut peers = vec![
        peer(50_000, "A", "A-1", now),
        peer(50_001, "B", "B-1", now),
        peer(50_002, "C", "C-1", now),
    ];
    for i in 0..peers.len() {
        peers[i].client.connect(now);
        pump(&mut host, &mut peers, now);
    }
    let states: Vec<_> = peers.iter().map(|p| p.client.state()).collect();
    assert_eq!(
        states,
        [
            SessionState::Established,
            SessionState::Established,
            SessionState::Idle
        ]
    );
    assert_eq!(host.session_count(), 2);
    // The limit is on concurrent Sessions: one that ends makes room.
    peers[0].client.close(now, ByeReason::UserTerminated);
    pump(&mut host, &mut peers, now);
    assert_eq!(host.session_count(), 1);
    peers[2].client.connect(now);
    pump(&mut host, &mut peers, now);
    assert_eq!(peers[2].client.state(), SessionState::Established);
    assert_eq!(host.session_count(), 2);
}

/// Spec: m2-124/8#5
#[test]
fn a_host_that_takes_no_more_sessions_still_answers_a_ping_from_a_stranger() {
    let now = Instant::now();
    let mut host = host_taking(1);
    let mut peers = vec![peer(50_000, "A", "A-1", now)];
    peers[0].client.connect(now);
    pump(&mut host, &mut peers, now);
    assert_eq!(host.session_count(), 1);

    // A Client that is not in a Session pings the full Host.
    let stranger: SocketAddr = "10.0.0.77:6000".parse().unwrap();
    host.on_message(stranger, &datagram(&Command::Ping(PingId::new(7))), now);
    let reply = host.poll_transmit().expect("the Ping is answered");
    assert_eq!(reply.to, stranger);
    assert_eq!(
        commands(&reply.payload),
        [Command::PingReply(PingId::new(7))]
    );
    assert_eq!(host.session_count(), 1, "the Ping opens no Session");

    // The same Client's Invitation is refused: the Ping got no Session.
    let invitation = Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("Late", "LATE-1"),
    });
    host.on_message(stranger, &datagram(&invitation), now);
    let refusal = host.poll_transmit().unwrap();
    assert_eq!(
        commands(&refusal.payload),
        [Command::Bye(Box::new(Bye::new(ByeReason::TooManySessions)))]
    );
    host.on_message(stranger, &datagram(&Command::Ping(PingId::new(8))), now);
    let again = host.poll_transmit().unwrap();
    assert_eq!(
        commands(&again.payload),
        [Command::PingReply(PingId::new(8))]
    );
}

/// A device that is a Host and a Client for one UMP Endpoint hands each role
/// the one identity; the TXT record, the Invitation and every Invitation Reply
/// carry it, and a Host that Client invites reads the Host's TXT identity back
/// from it.
/// Spec: m2-124/9#2, m2-124/9#3, m2-124/6.5#7, m2-124/6.5#9
#[test]
fn a_device_in_both_roles_presents_one_identity_in_the_txt_record_and_every_command() {
    let now = Instant::now();
    let device = identity("Workbench", "DEV-1");
    let mut seen = Vec::new();

    // Its Host's TXT record, as another device's Browser would read it.
    let [(_, name), (_, id)] = txt_properties(&device);
    let published = identity_from_txt(&name, &id).unwrap();
    seen.push(("TXT", published));

    // Its Client's Invitation, and the replies of its Host in each admission.
    let mut client = Client::new(
        device,
        Credentials::none(),
        SessionConfig::default(),
        host_addr(),
        now,
    );
    client.connect(now);
    let invitation = commands(&client.poll_transmit().unwrap().payload);
    let [Command::Invitation(inv)] = invitation.as_slice() else {
        panic!("{invitation:?}")
    };
    seen.push(("Invitation", inv.identity));

    let admissions = [
        Admission::Open,
        Admission::Confirm,
        Admission::SharedSecret("pw".to_owned()),
        Admission::Users(vec![(Username::new("Rosa").unwrap(), "pw".to_owned())]),
    ];
    let asker: SocketAddr = "10.0.0.5:5000".parse().unwrap();
    for admission in admissions {
        let mut config = HostConfig::new(device);
        config.admission = admission;
        let mut host = Host::new(config);
        let secret_methods = AuthMethods::SHARED_SECRET.union(AuthMethods::USER);
        let invite = Command::Invitation(Invitation {
            methods: secret_methods,
            identity: identity("Other", "OTHER-1"),
        });
        host.on_message(asker, &datagram(&invite), now);
        let reply = commands(&host.poll_transmit().unwrap().payload);
        let who = match reply.as_slice() {
            [Command::Accepted(i) | Command::Pending(i)] => *i,
            [Command::AuthRequired(c)] => c.identity,
            [Command::UserAuthRequired(c)] => c.identity,
            other => panic!("{other:?}"),
        };
        seen.push(("Reply", who));
    }
    assert_eq!(seen.len(), 6);
    for (what, who) in seen {
        assert_eq!(who, device, "{what} carries the device's identity");
    }
}

/// A Host that is asked again by a peer it already holds answers with its own
/// identity: a repeated Invitation, a repeated Invitation with Authentication
/// in a Session, and a repeated Invitation while the user is deciding.
/// Spec: m2-124/9#2
#[test]
fn a_repeated_invitation_is_answered_with_the_hosts_own_identity() {
    let now = Instant::now();
    let device = identity("Workbench", "DEV-1");
    let invite = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("Other", "OTHER-1"),
    }));
    let answer = |host: &mut Host, bytes: &[u8]| {
        host.on_message(PEER, bytes, now);
        let mut last = Vec::new();
        while let Some(t) = host.poll_transmit() {
            last = commands(&t.payload);
        }
        last
    };

    let mut open = Host::new(HostConfig::new(device));
    open.on_message(PEER, &invite, now);
    while open.poll_transmit().is_some() {}
    assert_eq!(answer(&mut open, &invite), [Command::Accepted(device)]);
    let auth = datagram(&Command::InvitationWithAuth(Digest::new([0; 32])));
    assert_eq!(answer(&mut open, &auth), [Command::Accepted(device)]);

    let mut config = HostConfig::new(device);
    config.admission = Admission::Confirm;
    let mut confirming = Host::new(config);
    confirming.on_message(PEER, &invite, now);
    while confirming.poll_transmit().is_some() {}
    assert_eq!(answer(&mut confirming, &invite), [Command::Pending(device)]);
}

const PEER: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 5)),
    5000,
);
