//! Network MIDI 2.0 as the session's MIDI (feature `midi-network`, native targets):
//! [`Midi::open_network`](crate::Midi::open_network) binds a Client or a Host and returns a [`MidiNetwork`].
//!
//! A network is an output as a [`MidiSink`] is: an output node whose events the
//! session sends each update ([`UmpOut`](crate::midi::UmpOut), the same drain), to
//! every peer a Session is established with. It is an input as a hardware port is,
//! one entry per peer: an input node
//! ([`MidiInputNode`]) fed by an event inlet, which
//! the update's `Phase::Poll` fills with the UMP the link received. A Client has one
//! peer, its Host; a Host has one per Client, each known by its
//! [`EndpointIdentity`].
//!
//! The session owns the link. The last clone of the handle dropped, the next update
//! retires the entries and drops the link on the executor's blocking pool: a Bye to
//! every peer, then the join of the socket task, which waits out the socket's read.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tutti_buffers::Float;
use tutti_graph::{EventInletWriter, Losses, NodeKey, Stamp};
use tutti_midi_network::{
    Admission, AuthFailure, ByeReason, Client, Credentials, EndedBy, EndpointIdentity, Event,
    FoundHost, Host, HostConfig, NetworkLink, SessionConfig,
};
use tutti_midi_runtime::{
    LIVE_INLET_CAPACITY, MidiInputNode, MidiInputPorts, MidiOut, Outgoing, Taken,
};
use tutti_platform::{Instant, WeakExecutor};
use tutti_rt::LockExt;
use tutti_ump::Ump;

use crate::graph::Graph;
use crate::midi::{Claim, Departure, Held, Retiring, settle};
use crate::{EventInPort, EventOutPort, MidiError, MidiReport, MidiSink, SessionEvent, Stamping};

// --- The request ---

/// Which end of a Session this end is.
#[derive(Clone, Debug)]
enum Role {
    /// Invites the Host at the address.
    Client(SocketAddr),
    /// Listens on the port.
    Host {
        port: u16,
        admission: Admission,
        max_sessions: Option<NonZeroUsize>,
    },
}

/// A network endpoint to open: [`Midi::open_network`](crate::Midi::open_network).
///
/// A Client ([`client`](Self::client), [`client_of`](Self::client_of)) invites a Host
/// and holds one Session; a Host ([`host`](Self::host)) serves any number of Clients.
/// Both present an [`EndpointIdentity`] (M2-124 section 4.4).
#[derive(Clone, Debug)]
#[must_use = "a request opens nothing until it is given to `Midi::open_network`"]
pub struct OpenNetwork {
    role: Role,
    identity: EndpointIdentity,
    credentials: Credentials,
    config: SessionConfig,
}

impl OpenNetwork {
    /// A Client called `identity` that invites the Host at `host`.
    pub fn client(identity: EndpointIdentity, host: SocketAddr) -> Self {
        Self::new(Role::Client(host), identity)
    }

    /// A Client called `identity` that invites `host`, a Host found by discovery
    /// ([`Browser`](tutti_midi_network::Browser)): at its first address. `None`
    /// when `host` lists no address.
    pub fn client_of(identity: EndpointIdentity, host: &FoundHost) -> Option<Self> {
        host.addresses
            .first()
            .map(|&address| Self::client(identity, address))
    }

    /// A Host called `identity` that listens on UDP `port` of every interface (`0`
    /// for one the OS picks: [`MidiNetwork::local_addr`] says which). It accepts
    /// every Invitation unless [`admission`](Self::admission) says otherwise.
    pub fn host(identity: EndpointIdentity, port: u16) -> Self {
        Self::new(
            Role::Host {
                port,
                admission: Admission::Open,
                max_sessions: None,
            },
            identity,
        )
    }

    fn new(role: Role, identity: EndpointIdentity) -> Self {
        Self {
            role,
            identity,
            credentials: Credentials::none(),
            config: SessionConfig::default(),
        }
    }

    /// What a Client answers a Host's authentication challenge with. A Host
    /// ignores it: its credentials are its [`admission`](Self::admission).
    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// How a Session sends, repeats and recovers.
    pub const fn config(mut self, config: SessionConfig) -> Self {
        self.config = config;
        self
    }

    /// How a Host admits an Invitation: with
    /// [`Admission::Confirm`] each one is reported
    /// ([`NetworkReport::Invited`]) and waits for
    /// [`MidiNetwork::accept`] or [`MidiNetwork::decline`]. A Client has no admission.
    pub fn admission(mut self, admission: Admission) -> Self {
        if let Role::Host {
            admission: held, ..
        } = &mut self.role
        {
            *held = admission;
        }
        self
    }

    /// How many Clients a Host serves at once. A Client has no such limit.
    pub fn max_sessions(mut self, max: NonZeroUsize) -> Self {
        if let Role::Host { max_sessions, .. } = &mut self.role {
            *max_sessions = Some(max);
        }
        self
    }
}

// --- What the network reports ---

/// What happened in a network endpoint's sessions, reported once from
/// `Phase::Poll` as [`MidiReport::Network`]. Each is about one peer.
#[derive(Debug)]
#[non_exhaustive]
pub enum NetworkReport {
    /// A Host that admits with [`Admission::Confirm`] was invited: it waits for
    /// [`MidiNetwork::accept`] or [`MidiNetwork::decline`].
    Invited {
        /// Who invited.
        identity: EndpointIdentity,
    },
    /// A Session is established with the peer: its input node is in the graph
    /// ([`MidiNetwork::input_node`]).
    Connected {
        /// The peer.
        identity: EndpointIdentity,
    },
    /// The Session, or the Invitation still waiting, ended: the peer's input
    /// entry is retired.
    Ended {
        /// The peer.
        identity: EndpointIdentity,
        /// Which end ended it.
        by: EndedBy,
        /// The reason given.
        reason: ByeReason,
    },
    /// A Client's authentication did not succeed. The identity is the peer's
    /// when it was told one.
    AuthenticationFailed {
        /// Where the peer is.
        peer: SocketAddr,
        /// Who it is, when known.
        identity: Option<EndpointIdentity>,
        /// Why.
        cause: AuthFailure,
    },
}

impl From<NetworkReport> for SessionEvent {
    fn from(report: NetworkReport) -> Self {
        MidiReport::Network(report).into()
    }
}

/// Why a [`MidiNetwork`] call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NetworkError {
    /// [`accept`](MidiNetwork::accept) or [`decline`](MidiNetwork::decline) on a
    /// Client: only a Host is invited.
    #[error("only a Host is invited")]
    NotAHost,
    /// No Invitation from this peer waits: it was answered, ended, or never came.
    #[error("no Invitation from that peer waits")]
    NoInvitation,
}

// --- The link ---

/// A Client's link or a Host's.
enum Link {
    Client(NetworkLink<Client>),
    Host(NetworkLink<Host>),
}

impl Link {
    fn pending(&self) -> Vec<Event> {
        match self {
            Self::Client(link) => link.pending().collect(),
            Self::Host(link) => link.pending().collect(),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Client(link) => link.local_addr(),
            Self::Host(link) => link.local_addr(),
        }
    }

    fn events_lost(&self) -> u64 {
        match self {
            Self::Client(link) => link.events_lost(),
            Self::Host(link) => link.events_lost(),
        }
    }

    fn sends_failed(&self) -> u64 {
        match self {
            Self::Client(link) => link.sends_failed(),
            Self::Host(link) => link.sends_failed(),
        }
    }

    /// Sends `packets` to the peers at `to` (a Client's one is its Host), and
    /// whether any Session took them.
    fn send(&self, to: &[SocketAddr], packets: &[Ump]) -> bool {
        match self {
            Self::Client(link) => link.with(|client, now| client.send(now, packets)).is_ok(),
            Self::Host(link) => link.with(|host, now| {
                to.iter()
                    .filter(|&&peer| host.send(now, peer, packets).is_ok())
                    .count()
                    > 0
            }),
        }
    }

    /// Answers the Invitation of `peer`.
    fn answer(&self, peer: SocketAddr, accept: bool) -> Result<(), NetworkError> {
        match self {
            Self::Client(_) => Err(NetworkError::NotAHost),
            Self::Host(link) => {
                link.with(|host, now| {
                    if accept {
                        host.accept(now, peer);
                    } else {
                        host.decline(now, peer);
                    }
                });
                Ok(())
            }
        }
    }
}

/// A peer the network knows: invited, or with a Session whose input node is in
/// the graph.
#[derive(Clone, Copy, Debug)]
struct Peer {
    addr: SocketAddr,
    identity: EndpointIdentity,
    /// The input node of an established peer; `None` while it is only invited.
    input: Option<NodeKey>,
}

/// What the session and the handles of one network share.
struct NetShared {
    link: Link,
    peers: Mutex<Vec<Peer>>,
}

impl std::fmt::Debug for NetShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetShared")
            .field("peers", &self.peers.lock_recover().len())
            .finish_non_exhaustive()
    }
}

impl NetShared {
    fn established(&self) -> Vec<SocketAddr> {
        self.peers
            .lock_recover()
            .iter()
            .filter(|peer| peer.input.is_some())
            .map(|peer| peer.addr)
            .collect()
    }
}

/// The sink the session's output drain sends to: the established peers of the
/// network, while it lives.
struct NetworkOut(Arc<OnceLock<Weak<NetShared>>>);

impl MidiOut for NetworkOut {
    /// Sent at once whatever each event's [`SendAt`](tutti_midi_runtime::SendAt): a
    /// Session has no wire clock. Nothing is taken when no Session is established.
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        let Some(shared) = self.0.get().and_then(Weak::upgrade) else {
            return Taken::NONE;
        };
        let packets: Vec<Ump> = events.iter().map(|event| event.ump).collect();
        if shared.link.send(&shared.established(), &packets) {
            Taken::within(events, events.len())
        } else {
            Taken::NONE
        }
    }
}

// --- The handle ---

/// A network endpoint: [`Midi::open_network`](crate::Midi::open_network).
///
/// `Clone` and `Send`. Clones share the endpoint, which closes when the last clone
/// is dropped: the next [`update`](crate::Session::update) retires the peers' input
/// nodes and the link says Bye to every peer.
#[derive(Clone, Debug)]
pub struct MidiNetwork {
    claim: Arc<Claim<NetShared>>,
    sink: MidiSink,
}

impl MidiNetwork {
    /// The output node: wire to its event input what should be sent to the peers
    /// ([`MidiSink::node`]). Each established peer is sent every event.
    #[must_use]
    pub fn output_node(&self) -> NodeKey {
        self.sink.node()
    }

    /// The input node of the established peer `identity`: what a peer sent comes
    /// out of its event outputs, as out of a hardware input's. `None` before
    /// [`NetworkReport::Connected`] and after [`NetworkReport::Ended`].
    #[must_use]
    pub fn input_node(&self, identity: EndpointIdentity) -> Option<NodeKey> {
        self.claim
            .shared
            .peers
            .lock_recover()
            .iter()
            .find(|peer| peer.identity == identity)
            .and_then(|peer| peer.input)
    }

    /// The established peers and their input nodes.
    #[must_use]
    pub fn inputs(&self) -> Vec<(EndpointIdentity, NodeKey)> {
        self.claim
            .shared
            .peers
            .lock_recover()
            .iter()
            .filter_map(|peer| peer.input.map(|node| (peer.identity, node)))
            .collect()
    }

    /// The address the socket is bound to: where a Host is invited, and the
    /// source address a Client's Host sees.
    ///
    /// # Errors
    ///
    /// The error from the socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.claim.shared.link.local_addr()
    }

    /// Accepts the Invitation of `identity` (a Host that admits with
    /// [`Admission::Confirm`], after [`NetworkReport::Invited`]): the Session is
    /// established with the next update.
    ///
    /// # Errors
    ///
    /// [`NetworkError::NotAHost`] on a Client, [`NetworkError::NoInvitation`] when
    /// `identity` has none waiting.
    pub fn accept(&self, identity: EndpointIdentity) -> Result<(), NetworkError> {
        self.answer(identity, true)
    }

    /// Declines the Invitation of `identity`; the peer is told
    /// ([`ByeReason::UserDeclined`]).
    ///
    /// # Errors
    ///
    /// As [`accept`](Self::accept).
    pub fn decline(&self, identity: EndpointIdentity) -> Result<(), NetworkError> {
        self.answer(identity, false)
    }

    fn answer(&self, identity: EndpointIdentity, accept: bool) -> Result<(), NetworkError> {
        let shared = &self.claim.shared;
        let addr = shared
            .peers
            .lock_recover()
            .iter()
            .find(|peer| peer.identity == identity && peer.input.is_none())
            .map(|peer| peer.addr);
        match (&shared.link, addr) {
            (Link::Client(_), _) => Err(NetworkError::NotAHost),
            (Link::Host(_), None) => Err(NetworkError::NoInvitation),
            (Link::Host(_), Some(peer)) => shared.link.answer(peer, accept),
        }
    }

    /// What the output did not send, cumulative: [`lost`](Losses::lost) are events
    /// its node's tap had no room for, [`refused`](Losses::refused) those no
    /// Session took (none was established, or the link had failed).
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.sink.losses()
    }

    /// How many events the link dropped before the session read them.
    #[must_use]
    pub fn events_lost(&self) -> u64 {
        self.claim.shared.link.events_lost()
    }

    /// How many datagrams the OS refused for their destination.
    #[must_use]
    pub fn sends_failed(&self) -> u64 {
        self.claim.shared.link.sends_failed()
    }
}

// --- The session's table of networks ---

/// One peer's input as the session keeps it.
struct Entry {
    addr: SocketAddr,
    identity: EndpointIdentity,
    /// The peer's input node.
    node: NodeKey,
    /// The event inlet feeding the node, and its one writer.
    inlet: NodeKey,
    writer: EventInletWriter,
    /// Moves the node's generation on when the entry retires.
    departure: Departure,
}

/// One network as the session keeps it.
struct Net {
    shared: Arc<NetShared>,
    claim: Held<NetShared>,
    entries: Vec<Entry>,
    executor: WeakExecutor,
}

/// The networks of the session, and the input nodes going.
#[derive(Default)]
pub(crate) struct Networks {
    nets: Vec<Net>,
    /// The input nodes of retired entries, taken out once the graph has rendered
    /// past the release of what they held.
    retiring: Vec<Retiring<()>>,
}

impl std::fmt::Debug for Networks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Networks")
            .field("nets", &self.nets.len())
            .finish_non_exhaustive()
    }
}

/// What a poll builds an input node with: the session's MPE mode and which Groups
/// an input hears.
#[derive(Clone, Copy)]
struct InputCx<'a> {
    mpe: &'a tutti_rt::Publisher<tutti_midi_protocol::MpeMode>,
    layout: MidiInputPorts,
}

impl Entry {
    /// Opens the input of `identity` at `addr`: its node joins the graph, fed by
    /// an event inlet.
    fn open<F: Float>(
        graph: &mut Graph<F>,
        cx: &InputCx<'_>,
        addr: SocketAddr,
        identity: EndpointIdentity,
    ) -> Self {
        let (departure, generation) = Departure::new();
        let node = graph
            .add(
                MidiInputNode::new()
                    .hearing(cx.layout)
                    .mpe(cx.mpe)
                    .following(&generation),
            )
            .erase();
        let (inlet, writer) = graph.open_event_inlet(LIVE_INLET_CAPACITY);
        let inlet = inlet.erase();
        // Both were added a moment ago, so both are in the graph.
        let wired = graph.set_sources(EventInPort::new(node, 0), [EventOutPort::new(inlet, 0)]);
        debug_assert!(
            wired.is_ok(),
            "the input node and its inlet were just added"
        );
        Self {
            addr,
            identity,
            node,
            inlet,
            writer,
            departure,
        }
    }
}

impl Networks {
    /// The input nodes of the peers, and of those going, for the routes to read.
    pub(crate) fn input_nodes(&self) -> Vec<NodeKey> {
        self.nets
            .iter()
            .flat_map(|net| net.entries.iter().map(|entry| entry.node))
            .chain(self.retiring.iter().map(|going| going.node))
            .collect()
    }

    /// Whether `node` is the input node of a peer.
    pub(crate) fn is_input_node(&self, node: NodeKey) -> bool {
        self.nets
            .iter()
            .any(|net| net.entries.iter().any(|entry| entry.node == node))
    }

    /// Retires `entry`, the one way a peer's input leaves: its inlet leaves the
    /// graph, and its node, which releases what the peer held, once the graph has
    /// rendered past the departure.
    fn retire<F: Float>(retiring: &mut Vec<Retiring<()>>, graph: &mut Graph<F>, mut entry: Entry) {
        // A host that removed the inlet itself leaves nothing to take.
        let _ = graph.remove(entry.inlet);
        retiring.push(Retiring {
            node: entry.node,
            going: (),
            generation: entry.departure.depart(),
        });
    }

    /// `Phase::Poll`: ends the networks whose handles were all dropped, takes the
    /// events each link queued (UMP to the peers' inlets, and each Session's life
    /// as a report), and takes out the input nodes that have released what their
    /// peers held. Whether the graph was edited.
    pub(crate) fn poll<F: Float>(
        &mut self,
        graph: &mut Graph<F>,
        mpe: &tutti_rt::Publisher<tutti_midi_protocol::MpeMode>,
        layout: MidiInputPorts,
        out: &mut Vec<SessionEvent>,
    ) -> bool {
        let cx = InputCx { mpe, layout };
        let mut edited = false;
        let mut at = 0;
        while at < self.nets.len() {
            if self.nets[at].claim.is_held() {
                edited |= self.nets[at].step(&mut self.retiring, graph, &cx, out);
                at += 1;
                continue;
            }
            let mut net = self.nets.remove(at);
            for entry in net.entries.drain(..) {
                Self::retire(&mut self.retiring, graph, entry);
            }
            net.release();
            edited = true;
        }
        let gone = settle(&mut self.retiring, graph, |_| {}, |_| false);
        edited || gone
    }
}

impl Net {
    /// Reads what the link queued. Whether the graph was edited.
    fn step<F: Float>(
        &mut self,
        retiring: &mut Vec<Retiring<()>>,
        graph: &mut Graph<F>,
        cx: &InputCx<'_>,
        out: &mut Vec<SessionEvent>,
    ) -> bool {
        let mut edited = false;
        for event in self.shared.link.pending() {
            match event {
                Event::Confirm { peer, remote } => {
                    self.shared.peers.lock_recover().push(Peer {
                        addr: peer,
                        identity: remote,
                        input: None,
                    });
                    out.push(NetworkReport::Invited { identity: remote }.into());
                }
                Event::Established { peer, remote } => {
                    // A peer that comes back under its identity from another
                    // address replaces its old entry.
                    for at in (0..self.entries.len()).rev() {
                        if self.entries[at].identity == remote || self.entries[at].addr == peer {
                            Networks::retire(retiring, graph, self.entries.remove(at));
                        }
                    }
                    let entry = Entry::open(graph, cx, peer, remote);
                    let mut peers = self.shared.peers.lock_recover();
                    peers.retain(|known| known.addr != peer && known.identity != remote);
                    peers.push(Peer {
                        addr: peer,
                        identity: remote,
                        input: Some(entry.node),
                    });
                    drop(peers);
                    self.entries.push(entry);
                    out.push(NetworkReport::Connected { identity: remote }.into());
                    edited = true;
                }
                Event::Ump { peer, packet } => {
                    // A peer with no Session has no input: its UMP goes nowhere.
                    if let Some(entry) = self.entries.iter_mut().find(|e| e.addr == peer) {
                        // A full inlet counts the loss itself; a removed node
                        // has nowhere to send.
                        let _ = entry.writer.push(Stamp::NextBlock, packet);
                    }
                }
                Event::Closed {
                    peer,
                    ended_by,
                    reason,
                } => {
                    let known = {
                        let mut peers = self.shared.peers.lock_recover();
                        let at = peers.iter().position(|known| known.addr == peer);
                        at.map(|at| peers.remove(at))
                    };
                    if let Some(at) = self.entries.iter().position(|e| e.addr == peer) {
                        Networks::retire(retiring, graph, self.entries.remove(at));
                        edited = true;
                    }
                    if let Some(known) = known {
                        out.push(
                            NetworkReport::Ended {
                                identity: known.identity,
                                by: ended_by,
                                reason,
                            }
                            .into(),
                        );
                    }
                }
                Event::AuthenticationFailed { peer, cause } => {
                    let identity = self
                        .shared
                        .peers
                        .lock_recover()
                        .iter()
                        .find(|known| known.addr == peer)
                        .map(|known| known.identity);
                    out.push(
                        NetworkReport::AuthenticationFailed {
                            peer,
                            identity,
                            cause,
                        }
                        .into(),
                    );
                }
                // The link counts what it lost; a lost run of UMP, a reset and a NAK
                // change nothing the session holds, and a nonce the OS could not
                // give refuses the Invitation, which the peer is told.
                Event::DataLost { .. }
                | Event::Reset { .. }
                | Event::Nak { .. }
                | Event::NonceUnavailable { .. } => {}
            }
        }
        edited
    }

    /// Drops the link on the executor's blocking pool: it sends a Bye to every peer
    /// and joins its socket task, which waits out the socket's read, so it is not
    /// done on the control thread. Where the executor is gone, here.
    fn release(self) {
        let Self {
            shared, executor, ..
        } = self;
        if let Some(executor) = executor.upgrade()
            && let Ok(task) = executor.unblock(move || drop(shared))
        {
            task.detach();
        }
    }
}

// --- Opening ---

impl<F: Float> crate::Midi<'_, F> {
    /// Opens the network endpoint `request`: binds its socket and returns at once.
    /// A Client's Invitation goes out and a Host listens; who joined, who left
    /// and what failed are [`NetworkReport`]s of the next updates.
    ///
    /// The link runs on the session's executor, and the session owns it: it ends
    /// when the last clone of the returned handle is dropped.
    ///
    /// # Errors
    ///
    /// [`MidiError::NetworkShutDown`] when the session's executor is gone, and
    /// [`MidiError::NetworkSocket`] when the socket cannot be bound or its task
    /// started. Nothing is opened.
    pub fn open_network(&mut self, request: OpenNetwork) -> Result<MidiNetwork, MidiError> {
        let executor = self.executor.upgrade().ok_or(MidiError::NetworkShutDown)?;
        let OpenNetwork {
            role,
            identity,
            credentials,
            config,
        } = request;
        let link = match role {
            Role::Client(host) => {
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
                let client = Client::new(identity, credentials, config, host, Instant::now());
                let link = NetworkLink::spawn(&executor, socket, client)?;
                link.with(|client, now| client.connect(now));
                Link::Client(link)
            }
            Role::Host {
                port,
                admission,
                max_sessions,
            } => {
                let mut host = HostConfig::new(identity);
                host.admission = admission;
                host.session = config;
                if let Some(max) = max_sessions {
                    host.max_sessions = max;
                }
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
                Link::Host(NetworkLink::spawn(&executor, socket, Host::new(host))?)
            }
        };
        // The sink is opened over a slot the link's state fills next: the session's
        // output drain reaches the link while the network lives and no longer.
        let slot = Arc::new(OnceLock::new());
        let sink = self.open_sink(Arc::new(NetworkOut(Arc::clone(&slot))), Stamping::Off);
        let shared = Arc::new(NetShared {
            link,
            peers: Mutex::new(Vec::new()),
        });
        // Set once, here.
        let _ = slot.set(Arc::downgrade(&shared));
        let (claim, held) = Held::new(Arc::clone(&shared), sink.node());
        self.state.networks.nets.push(Net {
            shared,
            claim: held,
            entries: Vec::new(),
            executor: self.executor.clone(),
        });
        Ok(MidiNetwork { claim, sink })
    }
}
