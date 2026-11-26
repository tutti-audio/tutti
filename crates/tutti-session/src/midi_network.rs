//! Network MIDI 2.0 as the session's MIDI (feature `midi-network`, native targets):
//! [`Midi::open_network_client`](crate::Midi::open_network_client) and
//! [`open_network_host`](crate::Midi::open_network_host) bind a Client or a Host and
//! return a [`MidiNetwork`].
//!
//! A network is an output as a [`MidiSink`] is, and an input as a hardware port is,
//! with one input entry per peer. The session owns the link.

use std::io;
use std::marker::PhantomData;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::num::NonZeroUsize;
use std::sync::{Arc, Weak};

use tutti_buffers::Float;
use tutti_graph::{EventInletWriter, NodeKey, Stamp};
use tutti_midi_device::{Capabilities, Endpoint, EndpointReply, Supports};
use tutti_midi_network::{
    Admission, AuthFailure, ByeReason, Client, Credentials, EndedBy, EndpointIdentity, Event,
    FoundHost, Host, HostConfig, NetworkEndpoint, NetworkLink, SessionConfig,
};
use tutti_midi_runtime::{
    LIVE_INLET_CAPACITY, MidiInputNode, MidiInputPorts, MidiOut, Outgoing, Taken,
};
use tutti_platform::{Executor, Instant};
use tutti_rt::Mutex;
use tutti_ump::{DeviceIdentity, Ump};
use tutti_units::Losses;

use crate::graph::Graph;
use crate::midi::{Claim, Departure, Held, Retiring, settle};
use crate::{
    EventInputPort, EventOutputPort, MidiError, MidiEvent, MidiSink, SessionEvent, Stamping,
};

/// An end of a Session a link runs: a [`Client`] or a [`Host`].
trait Role: NetworkEndpoint + Send + 'static {
    /// Sends `packets` to the Sessions with `to` (a Client's is its Host), and
    /// whether any took them.
    fn send(&mut self, now: Instant, to: &[SocketAddr], packets: &[Ump]) -> bool;

    /// Answers the Invitation of `peer`; only a Host is invited.
    fn answer(&mut self, _now: Instant, _peer: SocketAddr, _accept: bool) {}
}

impl Role for Client {
    fn send(&mut self, now: Instant, _: &[SocketAddr], packets: &[Ump]) -> bool {
        Self::send(self, now, packets).is_ok()
    }
}

impl Role for Host {
    fn send(&mut self, now: Instant, to: &[SocketAddr], packets: &[Ump]) -> bool {
        let mut took = false;
        for &peer in to {
            took |= Self::send(self, now, peer, packets).is_ok();
        }
        took
    }

    fn answer(&mut self, now: Instant, peer: SocketAddr, accept: bool) {
        if accept {
            self.accept(now, peer);
        } else {
            self.decline(now, peer);
        }
    }
}

/// What the session asks of a link, whichever role it runs.
trait Wire: Send + Sync {
    fn pending(&self) -> Box<dyn Iterator<Item = Event> + '_>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
    fn send(&self, to: &[SocketAddr], packets: &[Ump]) -> bool;
    fn answer(&self, peer: SocketAddr, accept: bool);
    fn close(&self);
}

impl<E: Role> Wire for NetworkLink<E> {
    fn pending(&self) -> Box<dyn Iterator<Item = Event> + '_> {
        Box::new(NetworkLink::pending(self))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        NetworkLink::local_addr(self)
    }

    fn send(&self, to: &[SocketAddr], packets: &[Ump]) -> bool {
        self.with(|endpoint, now| Role::send(endpoint, now, to, packets))
    }

    fn answer(&self, peer: SocketAddr, accept: bool) {
        self.with(|endpoint, now| endpoint.answer(now, peer, accept));
    }

    fn close(&self) {
        NetworkLink::close(self);
    }
}

/// A Client to open: [`Midi::open_network_client`](crate::Midi::open_network_client). It invites a
/// Host and holds one Session, which is the Host's.
#[derive(Clone, Debug)]
#[must_use = "a request opens nothing until it is given to `Midi::open_network_client`"]
pub struct OpenClient {
    identity: EndpointIdentity,
    device: Option<DeviceIdentity>,
    host: SocketAddr,
    credentials: Credentials,
    config: SessionConfig,
}

impl OpenClient {
    /// A Client called `identity` that invites the Host at `host`. Its socket is
    /// bound in `host`'s address family.
    pub fn new(identity: EndpointIdentity, host: SocketAddr) -> Self {
        Self {
            identity,
            device: None,
            host,
            credentials: Credentials::none(),
            config: SessionConfig::default(),
        }
    }

    /// A Client called `identity` that invites `host`, a Host found by discovery
    /// ([`Browser`](tutti_midi_network::Browser)): at its first IPv4 address, or its
    /// first address when it has none. `None` when `host` lists no address.
    pub fn client_of(identity: EndpointIdentity, host: &FoundHost) -> Option<Self> {
        host.addresses
            .iter()
            .find(|address| address.is_ipv4())
            .or_else(|| host.addresses.first())
            .map(|&address| Self::new(identity, address))
    }

    /// The Device Identity the Endpoint answers a peer's Endpoint Discovery with
    /// (M2-104 §7.1.3); with none, that part of a discovery goes unanswered. The
    /// name and the Product Instance Id it answers with are `identity`'s.
    pub const fn device(mut self, device: DeviceIdentity) -> Self {
        self.device = Some(device);
        self
    }

    /// What the Client answers a Host's authentication challenge with.
    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// The Sessions' [`SessionConfig`].
    pub const fn config(mut self, config: SessionConfig) -> Self {
        self.config = config;
        self
    }
}

/// A Host to open: [`Midi::open_network_host`](crate::Midi::open_network_host). It serves any
/// number of Clients, each with an input of its own.
#[derive(Clone, Debug)]
#[must_use = "a request opens nothing until it is given to `Midi::open_network_host`"]
pub struct OpenHost {
    identity: EndpointIdentity,
    device: Option<DeviceIdentity>,
    port: u16,
    admission: Admission,
    max_sessions: Option<NonZeroUsize>,
    config: SessionConfig,
}

impl OpenHost {
    /// A Host called `identity` that listens on IPv4 UDP `port` of every interface
    /// (`0` for one the OS picks: [`MidiNetwork::local_addr`] says which). It accepts
    /// every Invitation unless [`admission`](Self::admission) says otherwise.
    pub fn new(identity: EndpointIdentity, port: u16) -> Self {
        Self {
            identity,
            device: None,
            port,
            admission: Admission::Open,
            max_sessions: None,
            config: SessionConfig::default(),
        }
    }

    /// The Device Identity the Endpoint answers a peer's Endpoint Discovery with
    /// (M2-104 §7.1.3); with none, that part of a discovery goes unanswered. The
    /// name and the Product Instance Id it answers with are `identity`'s.
    pub const fn device(mut self, device: DeviceIdentity) -> Self {
        self.device = Some(device);
        self
    }

    /// How an Invitation is admitted: with [`Admission::Confirm`] each one is
    /// reported ([`NetworkEvent::Invited`]) and waits for
    /// [`MidiNetwork::accept`] or [`MidiNetwork::decline`].
    pub fn admission(mut self, admission: Admission) -> Self {
        self.admission = admission;
        self
    }

    /// How many Clients are served at once.
    pub const fn max_sessions(mut self, max: NonZeroUsize) -> Self {
        self.max_sessions = Some(max);
        self
    }

    /// The Sessions' [`SessionConfig`].
    pub const fn config(mut self, config: SessionConfig) -> Self {
        self.config = config;
        self
    }
}

impl OpenClient {
    fn spawn(self, executor: &Executor) -> io::Result<NetworkLink<Client>> {
        let any = if self.host.is_ipv6() {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        };
        let socket = UdpSocket::bind(any)?;
        let client = Client::new(
            self.identity,
            self.credentials,
            self.config,
            self.host,
            Instant::now(),
        );
        let link = NetworkLink::spawn(executor, socket, client)?;
        link.with(|client, now| client.connect(now));
        Ok(link)
    }
}

impl OpenHost {
    fn spawn(self, executor: &Executor) -> io::Result<NetworkLink<Host>> {
        let mut host = HostConfig::new(self.identity);
        host.admission = self.admission;
        host.session = self.config;
        if let Some(max) = self.max_sessions {
            host.max_sessions = max;
        }
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, self.port))?;
        NetworkLink::spawn(executor, socket, Host::new(host))
    }
}

/// What happened in a network endpoint's sessions, reported once from
/// `Phase::Poll` as [`MidiEvent::Network`]. Each is about one peer.
#[derive(Debug)]
#[non_exhaustive]
pub enum NetworkEvent {
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
    /// A new Session took the place of the peer's: it came back from another
    /// address, or another peer took its address. Its input entry is retired, and
    /// no Bye was sent. Reported before the new one's [`Connected`](Self::Connected).
    Replaced {
        /// The peer whose Session was replaced.
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
    /// A Client's authentication did not succeed.
    AuthenticationFailed {
        /// Where the peer is.
        peer: SocketAddr,
        /// Why.
        cause: AuthFailure,
    },
}

impl From<NetworkEvent> for SessionEvent {
    fn from(event: NetworkEvent) -> Self {
        MidiEvent::Network(event).into()
    }
}

/// Why a [`MidiNetwork<Host>`] call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NetworkError {
    /// The network holds no peer called that.
    #[error("the network holds no such peer")]
    UnknownPeer,
    /// The peer is known but no Invitation of it waits: it was answered already.
    #[error("no Invitation from that peer waits")]
    NoInvitation,
}

/// A peer the network knows: invited, or established.
#[derive(Clone, Copy, Debug)]
struct Peer {
    addr: SocketAddr,
    identity: EndpointIdentity,
    /// The input node of an established peer; `None` while it is only invited.
    input: Option<NodeKey>,
}

/// What the session and the handles of one network share.
struct NetShared {
    link: Box<dyn Wire>,
    peers: Mutex<Vec<Peer>>,
}

impl std::fmt::Debug for NetShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetShared")
            .field("peers", &self.peers.lock().len())
            .finish_non_exhaustive()
    }
}

impl NetShared {
    fn established(&self) -> Vec<SocketAddr> {
        self.peers
            .lock()
            .iter()
            .filter(|peer| peer.input.is_some())
            .map(|peer| peer.addr)
            .collect()
    }
}

/// The sink the session's output drain sends to: the established peers of the
/// network, while it lives.
struct NetworkOut(Weak<NetShared>);

impl MidiOut for NetworkOut {
    /// Sent at once whatever each event's [`SendAt`](tutti_midi_runtime::SendAt): a
    /// Session has no wire clock. Nothing is taken when no Session is established.
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        let Some(shared) = self.0.upgrade() else {
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

/// A network endpoint: [`Midi::open_network_client`](crate::Midi::open_network_client)
/// or [`open_network_host`](crate::Midi::open_network_host). `R` is the end of the
/// Session, [`Client`] or [`Host`]; only a `MidiNetwork<Host>` is invited,
/// so only it can [`accept`](Self::accept) or [`decline`](Self::decline).
///
/// `Clone` and `Send`. Clones share the endpoint, which closes when the last clone
/// is dropped: the next [`update`](crate::Session::update) retires the peers' input
/// nodes and drops the link, which sends a Bye to every peer and stops its socket
/// task without waiting for it.
pub struct MidiNetwork<R> {
    claim: Arc<Claim<NetShared>>,
    sink: MidiSink,
    role: PhantomData<fn() -> R>,
}

impl<R> Clone for MidiNetwork<R> {
    fn clone(&self) -> Self {
        Self {
            claim: Arc::clone(&self.claim),
            sink: self.sink.clone(),
            role: PhantomData,
        }
    }
}

impl<R> std::fmt::Debug for MidiNetwork<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiNetwork")
            .field("shared", &self.claim.shared)
            .finish_non_exhaustive()
    }
}

impl<R> MidiNetwork<R> {
    /// The output node: wire to its event input what should be sent to the peers
    /// ([`MidiSink::node`]). Each established peer is sent every event.
    #[must_use]
    pub fn output_node(&self) -> NodeKey {
        self.sink.node()
    }

    /// The input node of the established peer `identity`, whose event outputs carry
    /// what it sent as a hardware input's do. A query of the state now: `None` for
    /// a peer not established (not yet, no longer, or never).
    #[must_use]
    pub fn input_node(&self, identity: EndpointIdentity) -> Option<NodeKey> {
        let inputs = self.inputs();
        inputs
            .iter()
            .find(|(peer, _)| *peer == identity)
            .map(|&(_, node)| node)
    }

    /// The established peers and their input nodes.
    #[must_use]
    pub fn inputs(&self) -> Vec<(EndpointIdentity, NodeKey)> {
        self.claim
            .shared
            .peers
            .lock()
            .iter()
            .filter_map(|peer| peer.input.map(|node| (peer.identity, node)))
            .collect()
    }

    /// The address the socket is bound to.
    ///
    /// # Errors
    ///
    /// The error from the socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.claim.shared.link.local_addr()
    }

    /// What the output did not send, cumulative: [`lost`](Losses::lost) events its
    /// tap had no room for, [`refused`](Losses::refused) those no Session took.
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.sink.losses()
    }
}

impl MidiNetwork<Host> {
    /// Accepts the Invitation of `identity` (a Host that admits with
    /// [`Admission::Confirm`], after [`NetworkEvent::Invited`]): the Session is
    /// established with the next update.
    ///
    /// # Errors
    ///
    /// [`NetworkError::UnknownPeer`] when the network holds no peer called
    /// `identity`, [`NetworkError::NoInvitation`] when it is known but has no
    /// Invitation waiting.
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
        let addr = {
            let peers = shared.peers.lock();
            let peer = peers
                .iter()
                .find(|peer| peer.identity == identity)
                .ok_or(NetworkError::UnknownPeer)?;
            if peer.input.is_some() {
                return Err(NetworkError::NoInvitation);
            }
            peer.addr
        };
        shared.link.answer(addr, accept);
        Ok(())
    }
}

/// What the Endpoint of a network is, for every peer it answers: what the app
/// opened it as.
#[derive(Clone, Copy)]
struct Local {
    /// The name and Product Instance Id the TXT record and the Invitation carry.
    identity: EndpointIdentity,
    device: Option<DeviceIdentity>,
}

impl Local {
    /// A network is UMP and MIDI 2.0 alone: the Session carries no MIDI 1.0
    /// Protocol, and JR Timestamps are not a Stream configuration here.
    const SUPPORTS: Supports = Supports::of(Capabilities {
        midi1: false,
        midi2: true,
        rx_jr: false,
        tx_jr: false,
    });

    /// The Endpoint a peer's Stream requests are answered as, over the Groups
    /// the session's inputs hear.
    fn endpoint(&self, groups: tutti_ump::GroupSpan) -> Endpoint {
        let endpoint = Endpoint::local(Self::SUPPORTS, groups)
            .named(*self.identity.name())
            .instanced(*self.identity.instance_id());
        match self.device {
            Some(device) => endpoint.identified(device),
            None => endpoint,
        }
    }
}

/// One peer's input as the session keeps it.
struct Entry {
    addr: SocketAddr,
    identity: EndpointIdentity,
    /// What the peer's Endpoint Discovery, Function Block Discovery and Stream
    /// Configuration Request are answered as.
    endpoint: Endpoint,
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
    /// The network's output node: its sink's record is what a peer that joins is
    /// caught up from.
    sink: NodeKey,
    claim: Held<NetShared>,
    local: Local,
    entries: Vec<Entry>,
    /// The peers established since the last [`Networks::take_joined`].
    joined: Vec<Joined>,
    /// The replies the link did not take since the last [`Networks::take_refused`].
    refused: u64,
}

/// A peer that joined a network's output: a listener with no graph edit, which
/// the output's record catches up ([`send`](Self::send)).
pub(crate) struct Joined {
    sink: NodeKey,
    peer: SocketAddr,
    shared: Arc<NetShared>,
}

impl Joined {
    /// The output node the peer listens to.
    pub(crate) fn sink(&self) -> NodeKey {
        self.sink
    }

    /// Sends `packets` to this peer alone: the peers already listening heard
    /// them as they were sent. Whether the link took them (nothing to send is
    /// taken).
    pub(crate) fn send(&self, packets: &[Ump]) -> bool {
        packets.is_empty() || self.shared.link.send(&[self.peer], packets)
    }
}

/// The networks of the session, and the input nodes going.
#[derive(Default)]
pub(crate) struct Networks {
    nets: Vec<Net>,
    /// The input nodes of retired entries, taken out once the graph has rendered
    /// past the release of what they held.
    retiring: Vec<Retiring<()>>,
}

/// The session ends its links when it drops, whatever handles live: a Bye to every
/// peer, and a send through a handle after it finds no Session.
impl Drop for Networks {
    fn drop(&mut self) {
        for net in &self.nets {
            net.shared.link.close();
        }
    }
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
        endpoint: Endpoint,
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
        let wired = graph.set_sources(
            EventInputPort::new(node, 0),
            [EventOutputPort::new(inlet, 0)],
        );
        debug_assert!(
            wired.is_ok(),
            "the input node and its inlet were just added"
        );
        Self {
            addr,
            identity,
            endpoint,
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

    /// The peers established since the last call, each a new listener of its
    /// network's output.
    pub(crate) fn take_joined(&mut self) -> Vec<Joined> {
        self.nets
            .iter_mut()
            .flat_map(|net| std::mem::take(&mut net.joined))
            .collect()
    }

    /// The replies each network's link did not take since the last call, with
    /// the network's output node, whose sink counts them refused.
    pub(crate) fn take_refused(&mut self) -> Vec<(NodeKey, u64)> {
        self.nets
            .iter_mut()
            .filter(|net| net.refused > 0)
            .map(|net| (net.sink, std::mem::take(&mut net.refused)))
            .collect()
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
            // The link drops here: a Bye to every peer, and its task stops, unawaited.
            drop(net);
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
        let shared = Arc::clone(&self.shared);
        for event in shared.link.pending() {
            match event {
                Event::Confirm { peer, remote } => {
                    let mut peers = shared.peers.lock();
                    peers.retain(|known| known.addr != peer);
                    peers.push(Peer {
                        addr: peer,
                        identity: remote,
                        input: None,
                    });
                    drop(peers);
                    out.push(NetworkEvent::Invited { identity: remote }.into());
                }
                Event::Established { peer, remote } => {
                    // A peer that comes back under its identity from another address,
                    // or another that takes its address, replaces the old entry,
                    // which ends first.
                    for at in (0..self.entries.len()).rev() {
                        if self.entries[at].identity == remote || self.entries[at].addr == peer {
                            let old = self.entries.remove(at);
                            out.push(
                                NetworkEvent::Replaced {
                                    identity: old.identity,
                                }
                                .into(),
                            );
                            Networks::retire(retiring, graph, old);
                        }
                    }
                    let endpoint = self.local.endpoint(cx.layout.groups());
                    let entry = Entry::open(graph, cx, peer, remote, endpoint);
                    let mut peers = shared.peers.lock();
                    peers.retain(|known| known.addr != peer && known.identity != remote);
                    peers.push(Peer {
                        addr: peer,
                        identity: remote,
                        input: Some(entry.node),
                    });
                    drop(peers);
                    self.entries.push(entry);
                    self.joined.push(Joined {
                        sink: self.sink,
                        peer,
                        shared: Arc::clone(&shared),
                    });
                    out.push(NetworkEvent::Connected { identity: remote }.into());
                    edited = true;
                }
                Event::Ump { peer, packet } => {
                    // A peer with no Session has no input: its UMP goes nowhere.
                    let Some(entry) = self.entries.iter_mut().find(|e| e.addr == peer) else {
                        continue;
                    };
                    // The Endpoint answers a Stream request and the replies go
                    // back to the peer alone; whatever it does not answer (a
                    // Stream notification, a musical message) is the input's.
                    let mut replies = Vec::new();
                    let answered = packet.decode().is_ok_and(|message| {
                        !matches!(
                            entry.endpoint.reply(message, &mut replies),
                            EndpointReply::Ignored
                        )
                    });
                    if !answered {
                        // A full inlet counts the loss itself; a removed node
                        // has nowhere to send.
                        let _ = entry.writer.push(Stamp::NextBlock, packet);
                    } else if !shared.link.send(&[peer], &replies) {
                        // Not retried: the peer asks again.
                        self.refused += replies.len() as u64;
                    }
                }
                Event::Closed {
                    peer,
                    ended_by,
                    reason,
                } => {
                    let known = {
                        let mut peers = shared.peers.lock();
                        let at = peers.iter().position(|known| known.addr == peer);
                        at.map(|at| peers.remove(at))
                    };
                    if let Some(at) = self.entries.iter().position(|e| e.addr == peer) {
                        Networks::retire(retiring, graph, self.entries.remove(at));
                        edited = true;
                    }
                    if let Some(known) = known {
                        out.push(
                            NetworkEvent::Ended {
                                identity: known.identity,
                                by: ended_by,
                                reason,
                            }
                            .into(),
                        );
                    }
                }
                Event::AuthenticationFailed { peer, cause } => {
                    out.push(NetworkEvent::AuthenticationFailed { peer, cause }.into());
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
}

impl<F: Float> crate::Midi<'_, F> {
    /// Opens the Client `request`: binds its socket, sends its Invitation and
    /// returns at once. The Host's answer, and who joins, leaves or fails, are
    /// [`NetworkEvent`]s of the next updates.
    ///
    /// The link runs on the session's executor, and the session owns it: it ends
    /// when the last clone of the returned handle is dropped.
    ///
    /// # Errors
    ///
    /// [`MidiError::NetworkShutDown`] when the session's executor is gone, and
    /// [`MidiError::NetworkSocket`] when the socket cannot be bound or its task
    /// started. Nothing is opened.
    pub fn open_network_client(
        &mut self,
        request: OpenClient,
    ) -> Result<MidiNetwork<Client>, MidiError> {
        let executor = self.executor.upgrade().ok_or(MidiError::NetworkShutDown)?;
        let local = Local {
            identity: request.identity,
            device: request.device,
        };
        Ok(self.adopt(request.spawn(&executor)?, local))
    }

    /// Opens the Host `request`: binds its socket and returns at once. It listens;
    /// each Client that joins has an input of its own ([`NetworkEvent`]s say who).
    ///
    /// # Errors
    ///
    /// As [`open_network_client`](Self::open_network_client).
    pub fn open_network_host(&mut self, request: OpenHost) -> Result<MidiNetwork<Host>, MidiError> {
        let executor = self.executor.upgrade().ok_or(MidiError::NetworkShutDown)?;
        let local = Local {
            identity: request.identity,
            device: request.device,
        };
        Ok(self.adopt(request.spawn(&executor)?, local))
    }

    /// The session holds `link` and answers with its handle.
    fn adopt<E: Role>(&mut self, link: NetworkLink<E>, local: Local) -> MidiNetwork<E> {
        let shared = Arc::new(NetShared {
            link: Box::new(link),
            peers: Mutex::new(Vec::new()),
        });
        // The output drain reaches the link while the network lives and no longer.
        let sink = self.open_sink(Arc::new(NetworkOut(Arc::downgrade(&shared))), Stamping::Off);
        let (claim, held) = Held::new(Arc::clone(&shared), sink.node());
        self.state.networks.nets.push(Net {
            shared,
            sink: sink.node(),
            claim: held,
            local,
            entries: Vec::new(),
            joined: Vec::new(),
            refused: 0,
        });
        MidiNetwork {
            claim,
            sink,
            role: PhantomData,
        }
    }
}
