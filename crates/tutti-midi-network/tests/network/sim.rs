//! A simulated network between a Client and a Host: datagrams cross only when
//! the test lets them, and the test owns the clock.

use std::net::SocketAddr;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_midi_network::{
    Client, Command, Credentials, EndpointIdentity, Event, Host, HostConfig, NetworkEndpoint,
    Nonce, SessionConfig, Transmit, parse,
};
use tutti_ump::Ump;
use tutti_ump::{EndpointName, ProductInstanceId};

pub(crate) fn identity(name: &str, id: &str) -> EndpointIdentity {
    EndpointIdentity::new(
        EndpointName::new(name).unwrap(),
        ProductInstanceId::new(id).unwrap(),
    )
    .unwrap()
}

pub(crate) fn host_addr() -> SocketAddr {
    "10.0.0.1:5673".parse().unwrap()
}

pub(crate) fn client_addr() -> SocketAddr {
    "10.0.0.2:50000".parse().unwrap()
}

/// A nonce the tests know: the same every time.
pub(crate) fn fixed_nonce() -> Nonce {
    Nonce::new(*b"0123456789abcdef").unwrap()
}

/// The nonce source of a Host that issues [`fixed_nonce`] every time.
pub(crate) fn fixed_nonce_source() -> Result<Nonce, getrandom::Error> {
    Ok(fixed_nonce())
}

pub(crate) fn note_on(note: u8) -> Ump {
    Ump([0x4090_0000 | (u32::from(note) << 8), 0x8000_0000, 0, 0])
}

/// What the network does to a datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fate {
    Deliver,
    Drop,
    Twice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    ClientToHost,
    HostToClient,
}

pub(crate) struct Sim {
    pub now: Instant,
    pub host: Host,
    pub client: Client,
    /// Every datagram that was sent, in order, whatever became of it.
    pub log: Vec<(Direction, Vec<u8>)>,
    pub client_events: Vec<Event>,
    pub host_events: Vec<Event>,
    fate: Box<dyn FnMut(Direction, &[u8]) -> Fate>,
}

impl Sim {
    pub(crate) fn new(host: HostConfig, credentials: Credentials, client: SessionConfig) -> Self {
        Self::with_nonces(host, credentials, client, fixed_nonce_source)
    }

    pub(crate) fn with_nonces(
        host: HostConfig,
        credentials: Credentials,
        client: SessionConfig,
        nonces: fn() -> Result<Nonce, getrandom::Error>,
    ) -> Self {
        let now = Instant::now();
        Self {
            now,
            host: Host::with_nonce_source(host, nonces),
            client: Client::new(
                identity("Laptop", "CLIENT-1"),
                credentials,
                client,
                host_addr(),
                now,
            ),
            log: Vec::new(),
            client_events: Vec::new(),
            host_events: Vec::new(),
            fate: Box::new(|_, _| Fate::Deliver),
        }
    }

    pub(crate) fn open() -> Self {
        Self::new(
            HostConfig::new(identity("Studio", "HOST-1")),
            Credentials::none(),
            SessionConfig::default(),
        )
    }

    pub(crate) fn set_fate(&mut self, fate: impl FnMut(Direction, &[u8]) -> Fate + 'static) {
        self.fate = Box::new(fate);
    }

    /// Moves datagrams both ways until none is queued, then collects events.
    pub(crate) fn flush(&mut self) {
        loop {
            let mut moved = false;
            while let Some(t) = self.client.poll_transmit() {
                moved = true;
                self.cross(Direction::ClientToHost, t);
            }
            while let Some(t) = self.host.poll_transmit() {
                moved = true;
                self.cross(Direction::HostToClient, t);
            }
            if !moved {
                break;
            }
        }
        while let Some(e) = self.client.poll_event() {
            self.client_events.push(e);
        }
        while let Some(e) = self.host.poll_event() {
            self.host_events.push(e);
        }
    }

    fn cross(&mut self, dir: Direction, t: Transmit) {
        self.log.push((dir, t.payload.clone()));
        let copies = match (self.fate)(dir, &t.payload) {
            Fate::Deliver => 1,
            Fate::Drop => 0,
            Fate::Twice => 2,
        };
        for _ in 0..copies {
            match dir {
                Direction::ClientToHost => {
                    self.host.on_message(client_addr(), &t.payload, self.now);
                }
                Direction::HostToClient => {
                    self.client.on_message(host_addr(), &t.payload, self.now);
                }
            }
        }
    }

    /// Advances the clock to `by` later, running both endpoints' timeouts at
    /// each deadline on the way, so no due work is skipped.
    pub(crate) fn advance(&mut self, by: Duration) {
        let end = self.now + by;
        for _ in 0..1_000_000 {
            let next = [self.client.deadline(), self.host.deadline()]
                .into_iter()
                .flatten()
                .min()
                .filter(|&t| t <= end);
            let Some(at) = next else { break };
            self.now = at.max(self.now);
            self.client.on_timeout(self.now);
            self.host.on_timeout(self.now);
            self.flush();
        }
        assert!(self.now <= end, "advance ran away");
        self.now = end;
    }

    pub(crate) fn connect(&mut self) {
        self.client.connect(self.now);
        self.flush();
    }

    /// The commands of every logged datagram in one direction.
    pub(crate) fn commands(&self, dir: Direction) -> Vec<Command> {
        self.log
            .iter()
            .filter(|(d, _)| *d == dir)
            .flat_map(|(_, bytes)| parse(bytes).unwrap().map(|c| c.unwrap()))
            .collect()
    }

    /// The UMPs a side received, in order.
    pub(crate) fn umps(events: &[Event]) -> Vec<Ump> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Ump { packet, .. } => Some(*packet),
                _ => None,
            })
            .collect()
    }
}
