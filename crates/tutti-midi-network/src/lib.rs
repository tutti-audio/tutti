#![doc = include_str!("../README.md")]
//! Network MIDI 2.0 over UDP (M2-124).
//!
//! The crate has four layers, each usable without the ones above it.
//!
//! - The wire format: [`Command`] is one Command Packet as a value, written by
//!   [`Command::write`] and read by [`Command::parse`]; [`parse`] reads a UDP
//!   payload's signature and yields its commands, [`DatagramBuilder`] writes one.
//!   Every refusal is a named [`ParseError`] or [`SignatureError`].
//! - The session machines: [`Client`] and [`Host`] hold the session states of
//!   section 6, authentication, keep-alive Pings, Session Reset, forward error
//!   correction, retransmit and the idle declaration. They take datagrams and
//!   an [`Instant`](tutti_platform::Instant), and hand back [`Transmit`]s and
//!   [`Event`]s ([`NetworkEndpoint`]); they own no socket and read no clock.
//! - [`UdpDriver`] runs a [`NetworkEndpoint`] over a `std::net::UdpSocket`.
//! - [`Advertisement`] and [`Browser`] publish and find Hosts by mDNS.
//!
//! ```
//! use std::net::SocketAddr;
//! use tutti_platform::Instant;
//! use tutti_ump::{EndpointName, ProductInstanceId};
//! use tutti_midi_network::{
//!     Client, Credentials, EndpointIdentity, Event, Host, HostConfig, NetworkEndpoint,
//!     SessionConfig, SessionState,
//! };
//!
//! let identity = |name: &str, id: &str| {
//!     EndpointIdentity::new(EndpointName::new(name).unwrap(), ProductInstanceId::new(id).unwrap())
//!         .unwrap()
//! };
//! let (host_addr, client_addr): (SocketAddr, SocketAddr) =
//!     ("127.0.0.1:5673".parse().unwrap(), "127.0.0.1:50000".parse().unwrap());
//! let now = Instant::now();
//!
//! let mut host = Host::new(HostConfig::new(identity("Studio", "HOST-1")));
//! let mut client = Client::new(
//!     identity("Laptop", "CLIENT-1"),
//!     Credentials::none(),
//!     SessionConfig::default(),
//!     host_addr,
//!     now,
//! );
//! client.connect(now);
//! // The invitation travels to the host, the acceptance back.
//! while let Some(sent) = client.poll_transmit() {
//!     host.on_message(client_addr, &sent.payload, now);
//! }
//! while let Some(sent) = host.poll_transmit() {
//!     client.on_message(host_addr, &sent.payload, now);
//! }
//! assert_eq!(client.state(), SessionState::Established);
//! assert!(matches!(client.poll_event(), Some(Event::Established { .. })));
//! ```
//!
//! # Not here
//!
//! Neither machine is wired into the engine's MIDI device layer. A Session
//! carries UMPs; the UMP Stream conversation of Endpoint and Function Block
//! discovery is the application's, over [`Event::Ump`] and [`Client::send`].

#![forbid(unsafe_code)]

mod auth;
mod command;
mod config;
mod datagram;
mod discovery;
mod endpoint;
mod event;
mod link;
mod policy;
mod sequence;
mod session;
mod text;
mod udp;

pub use auth::{Digest, Nonce};
pub use command::{
    AuthChallenge, AuthMethods, AuthState, Bye, ByeReason, Command, EndpointIdentity, Header,
    Invitation, MAX_UMP_DATA_WORDS, Nak, NakReason, ParseError, PingId, RetransmitErrorReason,
    RetransmitRefusal, RetransmitRequest, UmpData, UmpDataError, UserAuthChallenge, UserAuthState,
    UserAuthentication,
};
pub use config::{
    Fec, IdleInterval, IdleIntervalOutOfRange, Requests, Retransmit, SessionConfig, Timer,
    TimerOutOfRange,
};
pub use datagram::{
    Commands, DatagramBuilder, MAX_DATAGRAM_BYTES, Refused, SIGNATURE, SignatureError, parse,
};
pub use discovery::{
    Advertisement, Browser, DiscoveryError, FoundHost, HostEvent, InstanceName, InstanceNameError,
    KEY_ENDPOINT_NAME, KEY_PRODUCT_INSTANCE_ID, Pending, SERVICE_TYPE, identity_from_txt,
    instance_name, txt_properties,
};
pub use endpoint::{Client, Host, HostConfig, MAX_QUEUED_EVENTS, NetworkEndpoint};
pub use event::{AuthFailure, EndedBy, Event, SendError, SessionState, Transmit};
pub use policy::{Admission, Credentials};
pub use sequence::SequenceNumber;
pub use text::{ByeText, NakText, Username};
pub use udp::UdpDriver;
