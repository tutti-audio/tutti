//! A Client and a Host over real UDP sockets on the loopback interface.

use std::time::Duration;

use tutti_platform::Instant;

use tutti_midi_network::{
    ByeReason, Client, Credentials, EndedBy, EndpointIdentity, Event, Host, HostConfig,
    SessionConfig, SessionState, UdpDriver,
};

use crate::sim::{identity, note_on};

/// Polls both drivers until `done` holds or the deadline passes.
fn run_until(
    host: &mut UdpDriver<Host>,
    client: &mut UdpDriver<Client>,
    events: &mut Vec<(&'static str, Event)>,
    mut done: impl FnMut(&[(&'static str, Event)]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !done(events) {
        host.poll(Duration::from_millis(5)).unwrap();
        client.poll(Duration::from_millis(5)).unwrap();
        while let Some(e) = host.poll_event() {
            events.push(("host", e));
        }
        while let Some(e) = client.poll_event() {
            events.push(("client", e));
        }
    }
    assert!(done(events), "timed out; events so far: {events:?}");
}

#[test]
fn a_session_runs_over_real_udp_sockets_from_invitation_to_bye() {
    let mut host = UdpDriver::bind(
        "127.0.0.1:0",
        Host::new(HostConfig::new(identity("Studio", "HOST-1"))),
    )
    .unwrap();
    let host_addr = host.local_addr().unwrap();
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_addr = socket.local_addr().unwrap();
    let mut client = UdpDriver::new(
        socket,
        Client::new(
            identity("Laptop", "CLIENT-1"),
            Credentials::none(),
            SessionConfig::default(),
            host_addr,
            Instant::now(),
        ),
    );
    let mut events = Vec::new();

    client.endpoint_mut().connect(Instant::now());
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter()
            .filter(|(_, ev)| matches!(ev, Event::Established { .. }))
            .count()
            == 2
    });
    assert_eq!(client.endpoint().state(), SessionState::Established);

    // Client to host and back.
    let notes = [note_on(60), note_on(64), note_on(67)];
    client.endpoint_mut().send(Instant::now(), &notes).unwrap();
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter()
            .filter(|(who, ev)| *who == "host" && matches!(ev, Event::Ump { .. }))
            .count()
            == 3
    });
    host.endpoint_mut()
        .send(Instant::now(), client_addr, &[note_on(72)])
        .unwrap();
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter().any(|(who, ev)| {
            *who == "client" && matches!(ev, Event::Ump { packet, .. } if *packet == note_on(72))
        })
    });
    let heard: Vec<_> = events
        .iter()
        .filter_map(|(who, ev)| match ev {
            Event::Ump { packet, .. } if *who == "host" => Some(*packet),
            _ => None,
        })
        .collect();
    assert_eq!(heard, notes, "UMPs arrive once and in order");

    client
        .endpoint_mut()
        .close(Instant::now(), ByeReason::UserTerminated);
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter()
            .filter(|(_, ev)| matches!(ev, Event::Closed { .. }))
            .count()
            == 2
    });
    assert!(events.iter().any(|(who, ev)| {
        *who == "host"
            && matches!(
                ev,
                Event::Closed {
                    ended_by: EndedBy::Remote,
                    reason: ByeReason::UserTerminated,
                    ..
                }
            )
    }));
    assert_eq!(client.endpoint().state(), SessionState::Idle);
}

fn established_count(events: &[(&'static str, Event)]) -> usize {
    events
        .iter()
        .filter(|(_, ev)| matches!(ev, Event::Established { .. }))
        .count()
}

/// A Client bound to its own socket.
fn client_driver(identity: EndpointIdentity, host: std::net::SocketAddr) -> UdpDriver<Client> {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let client = Client::new(
        identity,
        Credentials::none(),
        SessionConfig::default(),
        host,
        Instant::now(),
    );
    UdpDriver::new(socket, client)
}

/// A device that serves a Client and is a Client of another Host at once,
/// each role on its own socket and both under one identity.
/// Spec: m2-124/9#1
#[test]
fn a_device_serves_a_client_and_is_a_client_of_another_host_on_two_ports() {
    let device = identity("Workbench", "DEV-1");
    let mut other_host = UdpDriver::bind(
        "127.0.0.1:0",
        Host::new(HostConfig::new(identity("Studio", "HOST-1"))),
    )
    .unwrap();
    let mut device_host =
        UdpDriver::bind("127.0.0.1:0", Host::new(HostConfig::new(device))).unwrap();
    let mut device_client = client_driver(device, other_host.local_addr().unwrap());
    let mut visitor = client_driver(
        identity("Laptop", "CLIENT-1"),
        device_host.local_addr().unwrap(),
    );

    device_client.endpoint_mut().connect(Instant::now());
    visitor.endpoint_mut().connect(Instant::now());
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && established_count(&events) < 4 {
        for driver in [&mut other_host, &mut device_host] {
            driver.poll(Duration::from_millis(5)).unwrap();
            while let Some(e) = driver.poll_event() {
                events.push(("host", e));
            }
        }
        for driver in [&mut device_client, &mut visitor] {
            driver.poll(Duration::from_millis(5)).unwrap();
            while let Some(e) = driver.poll_event() {
                events.push(("client", e));
            }
        }
    }
    assert_eq!(established_count(&events), 4, "{events:?}");
    assert_eq!(device_client.endpoint().state(), SessionState::Established);
    assert_eq!(device_host.endpoint().session_count(), 1);
    assert_eq!(other_host.endpoint().session_count(), 1);
    // The device's Client told the other Host the identity its own Host has.
    assert!(events.iter().any(|(who, ev)| {
        *who == "host" && matches!(ev, Event::Established { remote, .. } if *remote == device)
    }));
}

/// A device with two UMP Endpoints runs a Host for each, each with its own
/// port and name and the Product Instance Id they share.
/// Spec: m2-124/10#1
#[test]
fn a_device_with_two_endpoints_runs_a_host_for_each_on_its_own_port_and_name() {
    let (keys, synth) = (identity("Keys", "DEV-1"), identity("Synth", "DEV-1"));
    let mut hosts: Vec<UdpDriver<Host>> = [keys, synth]
        .into_iter()
        .map(|id| UdpDriver::bind("127.0.0.1:0", Host::new(HostConfig::new(id))).unwrap())
        .collect();
    let mut clients: Vec<UdpDriver<Client>> = hosts
        .iter()
        .map(|h| client_driver(identity("Laptop", "CLIENT-1"), h.local_addr().unwrap()))
        .collect();
    for c in &mut clients {
        c.endpoint_mut().connect(Instant::now());
    }
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && established_count(&events) < 4 {
        for h in &mut hosts {
            h.poll(Duration::from_millis(5)).unwrap();
            while let Some(e) = h.poll_event() {
                events.push(("host", e));
            }
        }
        for c in &mut clients {
            c.poll(Duration::from_millis(5)).unwrap();
            while let Some(e) = c.poll_event() {
                events.push(("client", e));
            }
        }
    }
    assert_eq!(established_count(&events), 4, "{events:?}");
    // Each Client learned the name of the Endpoint behind the port it chose.
    assert_eq!(clients[0].endpoint().remote(), Some(keys));
    assert_eq!(clients[1].endpoint().remote(), Some(synth));
    assert_eq!(hosts[0].endpoint().session_count(), 1);
    assert_eq!(hosts[1].endpoint().session_count(), 1);
}
