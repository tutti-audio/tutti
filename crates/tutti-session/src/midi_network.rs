//! Network MIDI 2.0 as the session's MIDI (feature `midi-network`, native targets):
//! [`Midi::open_network_client`](crate::Midi::open_network_client) and
//! [`open_network_host`](crate::Midi::open_network_host) bind a Client or a Host and
//! return a [`MidiNetwork`].
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
//! The session owns the link. When the last clone of the handle is dropped, the next
//! update retires the entries and drops the link: a Bye to every peer, and the socket
//! task stops without being waited for.

use std::io;
use std::marker::PhantomData;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, Weak};

use tutti_buffers::Float;
use tutti_graph::{EventInletWriter, Losses, NodeKey, Stamp};
use tutti_midi_network::{
    Admission, AuthFailure, ByeReason, Client, Credentials, EndedBy, EndpointIdentity, Event,
    FoundHost, Host, HostConfig, NetworkEndpoint, NetworkLink, SessionConfig,
};
use tutti_midi_runtime::{
    LIVE_INLET_CAPACITY, MidiInputNode, MidiInputPorts, MidiOut, Outgoing, Taken,
};
use tutti_platform::{Executor, Instant};
use tutti_rt::LockExt;
use tutti_ump::Ump;

use crate::graph::Graph;
use crate::midi::{Claim, Departure, Held, Retiring, settle};
use crate::{EventInPort, EventOutPort, MidiError, MidiReport, MidiSink, SessionEvent, Stamping};

// --- The two roles ---

/// An end of a Session a link runs: a [`Client`] or a [`Host`].
trait Role: NetworkEndpoint + Send + 'static {
    /// Sends `packets` to the Sessions established with `to` (a Client sends to its
    /// Host and ignores `to`), and whether any took them.
    fn send(&mut self, now: Instant, to: &[SocketAddr], packets: &[Ump]) -> bool;

    /// Answers the Invitation of `peer`. Only a Host is invited: a Client does
    /// nothing, and no handle of a Client can ask.
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
    fn events_lost(&self) -> u64;
    fn sends_failed(&self) -> u64;
    fn send(&self, to: &[SocketAddr], packets: &[Ump]) -> bool;
    fn answer(&self, peer: SocketAddr, accept: bool);
}

impl<E: Role> Wire for NetworkLink<E> {
    fn pending(&self) -> Box<dyn Iterator<Item = Event> + '_> {
        Box::new(NetworkLink::pending(self))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        NetworkLink::local_addr(self)
    }

    fn events_lost(&self) -> u64 {
        NetworkLink::events_lost(self)
    }

    fn sends_failed(&self) -> u64 {
        NetworkLink::sends_failed(self)
    }

    fn send(&self, to: &[SocketAddr], packets: &[Ump]) -> bool {
        self.with(|endpoint, now| Role::send(endpoint, now, to, packets))
    }

    fn answer(&self, peer: SocketAddr, accept: bool) {
        self.with(|endpoint, now| endpoint.answer(now, peer, accept));
    }
}

// --- The requests ---

/// A Client to open: [`Midi::open_network_client`](crate::Midi::open_network_client). It invites a
/// Host and holds one Session, which is the Host's.
#[derive(Clone, Debug)]
#[must_use = "a request opens nothing until it is given to `Midi::open_network`"]
pub struct OpenClient {
    identity: EndpointIdentity,
    host: SocketAddr,
    credentials: Credentials,
    config: SessionConfig,
}

impl OpenClient {
    /// A Client called `identity` that invites the Host at `host`. Its socket is
    /// bound in `host`'s address family.
    pub fn client(identity: EndpointIdentity, host: SocketAddr) -> Self {
        Self {
            identity,
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
            .map(|&address| Self::client(identity, address))
    }

    /// What the Client answers a Host's authentication challenge with.
    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// How a Session sends, repeats and recovers.
    pub const fn config(mut self, config: SessionConfig) -> Self {
        self.config = config;
        self
    }
}

/// A Host to open: [`Midi::open_network_host`](crate::Midi::open_network_host). It serves any
/// number of Clients, each with an input of its own.
#[derive(Clone, Debug)]
#[must_use = "a request opens nothing until it is given to `Midi::open_network`"]
pub struct OpenHost {
    identity: EndpointIdentity,
    port: u16,
    admission: Admission,
    max_sessions: Option<NonZeroUsize>,
    config: SessionConfig,
}

impl OpenHost {
    /// A Host called `identity` that listens on IPv4 UDP `port` of every interface
    /// (`0` for one the OS picks: [`MidiNetwork::local_addr`] says which). It accepts
    /// every Invitation unless [`admission`](Self::admission) says otherwise.
    pub fn host(identity: EndpointIdentity, port: u16) -> Self {
        Self {
            identity,
            port,
            admission: Admission::Open,
            max_sessions: None,
            config: SessionConfig::default(),
        }
    }

    /// How an Invitation is admitted: with [`Admission::Confirm`] each one is
    /// reported ([`NetworkReport::Invited`]) and waits for
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

    /// How a Session sends, repeats and recovers.
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

/// The end of a Session a [`MidiNetwork`] is: a Client, which invited a Host.
#[derive(Clone, Copy, Debug)]
pub enum ClientEnd {}

/// The end of a Session a [`MidiNetwork`] is: a Host, which is invited.
#[derive(Clone, Copy, Debug)]
pub enum HostEnd {}

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
    /// entry is retired. A Session that a new one replaced (the peer came back
    /// from another address, or another peer took its address) ends with
    /// [`EndedBy::Remote`] and [`ByeReason::Unknown`], before the new one's
    /// [`Connected`](Self::Connected).
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

impl From<NetworkReport> for SessionEvent {
    fn from(report: NetworkReport) -> Self {
        MidiReport::Network(report).into()
    }
}

/// Why a [`MidiNetwork<HostEnd>`] call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NetworkError {
    /// The network holds no peer called that: it never invited, or its Session
    /// ended.
    #[error("the network holds no such peer")]
    UnknownPeer,
    /// The peer is known but no Invitation of it waits: it was answered already.
    #[error("no Invitation from that peer waits")]
    NoInvitation,
}

// --- What the session and the handles share ---

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
    link: Box<dyn Wire>,
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

// --- The handle ---

/// A network endpoint: [`Midi::open_network_client`](crate::Midi::open_network_client)
/// or [`open_network_host`](crate::Midi::open_network_host). `R` is the end of the
/// Session, [`ClientEnd`] or [`HostEnd`]; only a `MidiNetwork<HostEnd>` is invited,
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

    /// The input node of the established peer `identity`: what a peer sent comes
    /// out of its event outputs, as out of a hardware input's. A query of the
    /// network's state now, so `None` for a peer not established: before its
    /// [`NetworkReport::Connected`], after its [`NetworkReport::Ended`], or never.
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

impl MidiNetwork<HostEnd> {
    /// Accepts the Invitation of `identity` (a Host that admits with
    /// [`Admission::Confirm`], after [`NetworkReport::Invited`]): the Session is
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
            let peers = shared.peers.lock_recover();
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
                    let mut peers = shared.peers.lock_recover();
                    peers.retain(|known| known.addr != peer);
                    peers.push(Peer {
                        addr: peer,
                        identity: remote,
                        input: None,
                    });
                    drop(peers);
                    out.push(NetworkReport::Invited { identity: remote }.into());
                }
                Event::Established { peer, remote } => {
                    // A peer that comes back under its identity from another address,
                    // or another that takes its address, replaces the old entry,
                    // which ends first.
                    for at in (0..self.entries.len()).rev() {
                        if self.entries[at].identity == remote || self.entries[at].addr == peer {
                            let old = self.entries.remove(at);
                            out.push(
                                NetworkReport::Ended {
                                    identity: old.identity,
                                    by: EndedBy::Remote,
                                    reason: ByeReason::Unknown,
                                }
                                .into(),
                            );
                            Networks::retire(retiring, graph, old);
                        }
                    }
                    let entry = Entry::open(graph, cx, peer, remote);
                    let mut peers = shared.peers.lock_recover();
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
                        let mut peers = shared.peers.lock_recover();
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
                    out.push(NetworkReport::AuthenticationFailed { peer, cause }.into());
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

// --- Opening ---

impl<F: Float> crate::Midi<'_, F> {
    /// Opens the Client `request`: binds its socket, sends its Invitation and
    /// returns at once. The Host's answer, and who joins, leaves or fails, are
    /// [`NetworkReport`]s of the next updates.
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
    ) -> Result<MidiNetwork<ClientEnd>, MidiError> {
        let executor = self.executor.upgrade().ok_or(MidiError::NetworkShutDown)?;
        Ok(self.adopt(request.spawn(&executor)?))
    }

    /// Opens the Host `request`: binds its socket and returns at once. It listens;
    /// who joins, leaves or fails to authenticate are [`NetworkReport`]s of the
    /// next updates, and each Client has an input of its own.
    ///
    /// The link runs on the session's executor, and the session owns it: it ends
    /// when the last clone of the returned handle is dropped.
    ///
    /// # Errors
    ///
    /// As [`open_network_client`](Self::open_network_client).
    pub fn open_network_host(
        &mut self,
        request: OpenHost,
    ) -> Result<MidiNetwork<HostEnd>, MidiError> {
        let executor = self.executor.upgrade().ok_or(MidiError::NetworkShutDown)?;
        Ok(self.adopt(request.spawn(&executor)?))
    }

    /// The session holds `link` and answers with its handle.
    fn adopt<E: Role, R>(&mut self, link: NetworkLink<E>) -> MidiNetwork<R> {
        let shared = Arc::new(NetShared {
            link: Box::new(link),
            peers: Mutex::new(Vec::new()),
        });
        // The output drain reaches the link while the network lives and no longer.
        let sink = self.open_sink(Arc::new(NetworkOut(Arc::downgrade(&shared))), Stamping::Off);
        let (claim, held) = Held::new(Arc::clone(&shared), sink.node());
        self.state.networks.nets.push(Net {
            shared,
            claim: held,
            entries: Vec::new(),
        });
        MidiNetwork {
            claim,
            sink,
            role: PhantomData,
        }
    }
}
