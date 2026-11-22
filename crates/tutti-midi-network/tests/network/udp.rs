//! Links over real UDP sockets on the loopback interface.

use std::collections::VecDeque;
use std::time::Duration;

use futures_lite::StreamExt;
use tutti_fixtures::block_on_within;
use tutti_platform::{Executor, Instant};

use tutti_midi_network::{
    Admission, ByeReason, Client, Credentials, EndedBy, EndpointIdentity, Event, Host, HostConfig,
    MAX_QUEUED_EVENTS, NetworkEndpoint, NetworkLink, SessionConfig, SessionState,
};

use crate::sim::{identity, note_on};

/// A hang guard: a wait that ends in time never reaches it.
const HANG_GUARD: Duration = Duration::from_secs(30);

/// The next event of `link` that `want` takes, skipping the others.
fn next_event<E: NetworkEndpoint + Send + 'static, T>(
    link: &NetworkLink<E>,
    mut want: impl FnMut(&Event) -> Option<T>,
) -> T {
    let mut events = link.events();
    loop {
        let event = block_on_within(events.next(), HANG_GUARD)
            .expect("an event within the hang guard")
            .expect("the socket task is running");
        if let Some(found) = want(&event) {
            return found;
        }
    }
}

fn established<E: NetworkEndpoint + Send + 'static>(link: &NetworkLink<E>) -> EndpointIdentity {
    next_event(link, |e| match e {
        Event::Established { remote, .. } => Some(*remote),
        _ => None,
    })
}

fn host(executor: &Executor, config: HostConfig) -> NetworkLink<Host> {
    NetworkLink::bind(executor, "127.0.0.1:0", Host::new(config)).unwrap()
}

/// A Client link that has invited the Host at `host`.
fn client(
    executor: &Executor,
    identity: EndpointIdentity,
    host: std::net::SocketAddr,
) -> NetworkLink<Client> {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let client = Client::new(
        identity,
        Credentials::none(),
        SessionConfig::default(),
        host,
        Instant::now(),
    );
    let link = NetworkLink::spawn(executor, socket, client).unwrap();
    link.with(|c, now| c.connect(now));
    link
}

/// An invitation to a confirming Host reaches the application as a Confirm
/// event, the application's accept establishes the Session at both ends, UMP
/// arrives once and in order each way, and a Bye ends it at both.
/// Spec: m2-124/6.4#1, m2-124/6.5#1, m2-124/6.16#1
#[test]
fn a_session_runs_over_two_links_from_invitation_to_bye() {
    let executor = Executor::new();
    let mut config = HostConfig::new(identity("Studio", "HOST-1"));
    config.admission = Admission::Confirm;
    let host = host(&executor, config);
    let client = client(
        &executor,
        identity("Laptop", "CLIENT-1"),
        host.local_addr().unwrap(),
    );
    let client_addr = client.local_addr().unwrap();

    let (peer, remote) = next_event(&host, |e| match e {
        Event::Confirm { peer, remote } => Some((*peer, *remote)),
        _ => None,
    });
    assert_eq!(peer, client_addr);
    assert_eq!(remote, identity("Laptop", "CLIENT-1"));
    host.with(|h, now| h.accept(now, peer));
    assert_eq!(established(&host), identity("Laptop", "CLIENT-1"));
    assert_eq!(established(&client), identity("Studio", "HOST-1"));
    assert_eq!(client.with(|c, _| c.state()), SessionState::Established);

    // Client to host and back.
    let notes = [note_on(60), note_on(64), note_on(67)];
    client.with(|c, now| c.send(now, &notes)).unwrap();
    let heard: Vec<_> = (0..notes.len())
        .map(|_| {
            next_event(&host, |e| match e {
                Event::Ump { packet, .. } => Some(*packet),
                _ => None,
            })
        })
        .collect();
    assert_eq!(heard, notes, "UMPs arrive once and in order");
    host.with(|h, now| h.send(now, client_addr, &[note_on(72)]))
        .unwrap();
    let back = next_event(&client, |e| match e {
        Event::Ump { packet, .. } => Some(*packet),
        _ => None,
    });
    assert_eq!(back, note_on(72));

    client.with(|c, now| c.close(now, ByeReason::UserTerminated));
    let (ended_by, reason) = next_event(&host, |e| match e {
        Event::Closed {
            ended_by, reason, ..
        } => Some((*ended_by, *reason)),
        _ => None,
    });
    assert_eq!(
        (ended_by, reason),
        (EndedBy::Remote, ByeReason::UserTerminated)
    );
    let (ended_by, _) = next_event(&client, |e| match e {
        Event::Closed {
            ended_by, reason, ..
        } => Some((*ended_by, *reason)),
        _ => None,
    });
    assert_eq!(ended_by, EndedBy::Local);
    assert_eq!(client.with(|c, _| c.state()), SessionState::Idle);
}

/// A device that serves a Client and is a Client of another Host at once,
/// each role on its own socket and both under one identity.
/// Spec: m2-124/9#1
#[test]
fn a_device_serves_a_client_and_is_a_client_of_another_host_on_two_ports() {
    let executor = Executor::new();
    let device = identity("Workbench", "DEV-1");
    let other_host = host(&executor, HostConfig::new(identity("Studio", "HOST-1")));
    let device_host = host(&executor, HostConfig::new(device));
    let device_client = client(&executor, device, other_host.local_addr().unwrap());
    let visitor = client(
        &executor,
        identity("Laptop", "CLIENT-1"),
        device_host.local_addr().unwrap(),
    );

    // The device's Client told the other Host the identity its own Host has.
    assert_eq!(established(&other_host), device);
    assert_eq!(established(&device_host), identity("Laptop", "CLIENT-1"));
    established(&device_client);
    established(&visitor);
    assert_eq!(
        device_client.with(|c, _| c.state()),
        SessionState::Established
    );
    assert_eq!(device_host.with(|h, _| h.session_count()), 1);
    assert_eq!(other_host.with(|h, _| h.session_count()), 1);
}

/// A device with two UMP Endpoints runs a Host for each, each with its own
/// port and name and the Product Instance Id they share.
/// Spec: m2-124/10#1
#[test]
fn a_device_with_two_endpoints_runs_a_host_for_each_on_its_own_port_and_name() {
    let executor = Executor::new();
    let (keys, synth) = (identity("Keys", "DEV-1"), identity("Synth", "DEV-1"));
    let hosts = [keys, synth].map(|id| host(&executor, HostConfig::new(id)));
    let clients = hosts.each_ref().map(|h| {
        client(
            &executor,
            identity("Laptop", "CLIENT-1"),
            h.local_addr().unwrap(),
        )
    });
    for h in &hosts {
        established(h);
    }
    for c in &clients {
        established(c);
    }
    // Each Client learned the name of the Endpoint behind the port it chose.
    assert_eq!(clients[0].with(|c, _| c.remote()), Some(keys));
    assert_eq!(clients[1].with(|c, _| c.remote()), Some(synth));
    assert_eq!(hosts[0].with(|h, _| h.session_count()), 1);
    assert_eq!(hosts[1].with(|h, _| h.session_count()), 1);
}

/// A [`NetworkEndpoint`] whose `on_message` panics: its socket task ends.
#[derive(Debug)]
struct Panics;

impl NetworkEndpoint for Panics {
    fn on_message(&mut self, _: std::net::SocketAddr, _: &[u8], _: Instant) {
        panic!("the endpoint refuses every datagram");
    }
    fn on_timeout(&mut self, _: Instant) {}
    fn poll_transmit(&mut self) -> Option<tutti_midi_network::Transmit> {
        None
    }
    fn poll_event(&mut self) -> Option<Event> {
        None
    }
    fn events_lost(&self) -> u64 {
        0
    }
    fn deadline(&self) -> Option<Instant> {
        None
    }
    fn close_all(&mut self, _: Instant) {}
}

/// A stream whose link's socket task has ended yields `None`, and `None`
/// again, with the cause in `failure`.
#[test]
fn a_stream_whose_task_ended_yields_none_and_keeps_yielding_none() {
    let executor = Executor::new();
    let link = NetworkLink::bind(&executor, "127.0.0.1:0", Panics).unwrap();
    assert!(link.failure().is_none(), "the task is running");
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .send_to(b"x", link.local_addr().unwrap())
        .unwrap();
    let mut events = link.events();
    assert_eq!(block_on_within(events.next(), HANG_GUARD), Some(None));
    assert_eq!(block_on_within(events.next(), HANG_GUARD), Some(None));
    assert!(link.failure().is_some());
}

/// `pending` takes what is queued and returns without waiting: an idle link
/// has nothing, and what a Session queued is taken once.
#[test]
fn pending_takes_only_what_is_queued_and_does_not_wait() {
    let executor = Executor::new();
    let host = host(&executor, HostConfig::new(identity("Studio", "HOST-1")));
    assert_eq!(host.pending().count(), 0, "nothing queued, nothing awaited");
    let client = client(
        &executor,
        identity("Laptop", "CLIENT-1"),
        host.local_addr().unwrap(),
    );
    // Wait for the Host's Established, which leaves nothing else queued ahead
    // of the UMP that follows.
    established(&host);
    established(&client);
    client.with(|c, now| c.send(now, &[note_on(60)])).unwrap();
    let mut got = Vec::new();
    let hang_guard = Instant::now() + HANG_GUARD;
    while got.is_empty() {
        assert!(Instant::now() < hang_guard, "the UMP never arrived");
        got.extend(host.pending());
        std::thread::yield_now();
    }
    assert!(matches!(&got[..], [Event::Ump { packet, .. }] if *packet == note_on(60)));
    assert_eq!(host.pending().count(), 0, "a taken event is not read again");
}

/// A [`NetworkEndpoint`] that yields `owed` events, one per `poll_event`.
#[derive(Debug, Default)]
struct Flood {
    owed: usize,
}

impl NetworkEndpoint for Flood {
    fn on_message(&mut self, _: std::net::SocketAddr, _: &[u8], _: Instant) {}
    fn on_timeout(&mut self, _: Instant) {}
    fn poll_transmit(&mut self) -> Option<tutti_midi_network::Transmit> {
        None
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.owed = self.owed.checked_sub(1)?;
        Some(Event::Reset {
            peer: "127.0.0.1:9".parse().unwrap(),
        })
    }
    fn events_lost(&self) -> u64 {
        0
    }
    fn deadline(&self) -> Option<Instant> {
        None
    }
    fn close_all(&mut self, _: Instant) {}
}

/// Events past the link's queue bound are counted lost, exactly the excess,
/// and the queue keeps its bound.
#[test]
fn events_past_the_queue_bound_are_counted_lost_exactly() {
    let executor = Executor::new();
    let link = NetworkLink::bind(&executor, "127.0.0.1:0", Flood::default()).unwrap();
    let excess = 7;
    link.with(|flood, _| flood.owed = MAX_QUEUED_EVENTS + excess);
    assert_eq!(link.events_lost(), excess as u64);
    assert_eq!(link.pending().count(), MAX_QUEUED_EVENTS);
}

/// Dropping a Client's link sends its Bye: the Host sees the Session end
/// from the remote side.
/// Spec: m2-124/6.16#1
#[test]
fn dropping_a_client_link_ends_its_session_at_the_host() {
    let executor = Executor::new();
    let host = host(&executor, HostConfig::new(identity("Studio", "HOST-1")));
    let client = client(
        &executor,
        identity("Laptop", "CLIENT-1"),
        host.local_addr().unwrap(),
    );
    established(&host);
    established(&client);
    drop(client);
    let (ended_by, reason) = next_event(&host, |e| match e {
        Event::Closed {
            ended_by, reason, ..
        } => Some((*ended_by, *reason)),
        _ => None,
    });
    assert_eq!((ended_by, reason), (EndedBy::Remote, ByeReason::PowerDown));
}

/// Dropping a Host's link sends a Bye to each Client it serves.
/// Spec: m2-124/6.16#1
#[test]
fn dropping_a_host_link_ends_every_session_at_its_clients() {
    let executor = Executor::new();
    let host = host(&executor, HostConfig::new(identity("Studio", "HOST-1")));
    let clients = ["CLIENT-1", "CLIENT-2"].map(|id| {
        client(
            &executor,
            identity("Laptop", id),
            host.local_addr().unwrap(),
        )
    });
    for c in &clients {
        established(c);
    }
    drop(host);
    for c in &clients {
        let closed = next_event(c, |e| match e {
            Event::Closed {
                ended_by, reason, ..
            } => Some((*ended_by, *reason)),
            _ => None,
        });
        assert_eq!(closed, (EndedBy::Remote, ByeReason::PowerDown));
    }
}

/// A [`NetworkEndpoint`] that reports each datagram it is given as a `Reset`
/// from the sender, and wants the clock at once when told to.
#[derive(Debug, Default)]
struct Echo {
    due: Option<Instant>,
    heard: VecDeque<Event>,
}

impl NetworkEndpoint for Echo {
    fn on_message(&mut self, from: std::net::SocketAddr, _: &[u8], _: Instant) {
        self.heard.push_back(Event::Reset { peer: from });
    }
    fn on_timeout(&mut self, _: Instant) {
        self.due = None;
    }
    fn poll_transmit(&mut self) -> Option<tutti_midi_network::Transmit> {
        None
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.heard.pop_front()
    }
    fn events_lost(&self) -> u64 {
        0
    }
    fn deadline(&self) -> Option<Instant> {
        self.due
    }
    fn close_all(&mut self, _: Instant) {}
}

/// A zero-length datagram from another port reaches the endpoint; the link's
/// own poke, which a deadline set through `with` sends, does not.
#[test]
fn a_zero_length_datagram_from_another_port_reaches_the_endpoint_but_the_pokes_do_not() {
    let executor = Executor::new();
    let link = NetworkLink::bind(&executor, "127.0.0.1:0", Echo::default()).unwrap();
    let to = link.local_addr().unwrap();
    let other = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let other_addr = other.local_addr().unwrap();
    let from_other = |event: &Event| match event {
        Event::Reset { peer } => Some(*peer),
        _ => None,
    };

    other.send_to(&[], to).unwrap();
    assert_eq!(next_event(&link, from_other), other_addr);

    // The poke goes out inside `with`, before the next datagram is sent, and
    // loopback keeps their order: were the poke fed to the endpoint, its
    // event would come first.
    link.with(|echo, now| echo.due = Some(now));
    other.send_to(b"x", to).unwrap();
    assert_eq!(next_event(&link, from_other), other_addr);
}

/// Closing a link says Bye to its peer and ends its Session: the peer sees the Bye
/// from the closing end, and a send after the close finds no Session established.
/// Closing again changes nothing.
/// Spec: m2-124/6.16#1
#[test]
fn close_says_bye_and_a_send_after_it_fails() {
    let executor = Executor::new();
    let host = host(&executor, HostConfig::new(identity("Studio", "HOST-1")));
    let client = client(
        &executor,
        identity("Laptop", "CLIENT-1"),
        host.local_addr().unwrap(),
    );
    established(&host);
    established(&client);

    client.close();
    let (ended_by, reason) = next_event(&host, |e| match e {
        Event::Closed {
            ended_by, reason, ..
        } => Some((*ended_by, *reason)),
        _ => None,
    });
    assert_eq!((ended_by, reason), (EndedBy::Remote, ByeReason::PowerDown));
    assert!(
        client.with(|c, now| c.send(now, &[note_on(60)])).is_err(),
        "no Session is established after a close"
    );
    client.close();
}
