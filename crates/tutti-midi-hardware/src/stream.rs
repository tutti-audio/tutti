//! What a device's UMP Stream conversation settled: [`DeviceStream`], and the
//! [`Negotiation`] that runs it (M2-104 §7.1.6.1, Table 4 steps 2 to 4).
//!
//! A [`EndpointDiscovery`] is the pure state machine; this is the part that has a
//! wire. One runs for each input the session opens on a device that is a UMP
//! Endpoint and has an output to send on:
//!
//! 1. The first [`poll`](Negotiation::poll) starts it: the Endpoint Discovery
//!    goes out on the device's output.
//! 2. Each poll hands the connection the Stream messages the device sent since
//!    (the port's stream ring, [`HardwareMidiInputs::drain_stream`](crate::HardwareMidiInputs::drain_stream)) and the
//!    time; what it answers goes out, and a step with no answer is sent again
//!    after [`STEP_TIMEOUT`](tutti_midi_device::STEP_TIMEOUT) and given up after [`MAX_ATTEMPTS`](tutti_midi_device::MAX_ATTEMPTS).
//! 3. It ends as a [`DeviceStream`].
//!
//! MIDI-CI then runs beside it for the life of the input: see the `ci` module's
//! docs. It is stepped by the same
//! [`poll`](Negotiation::poll), and what it finds is returned with the stream.
//!
//! Time is an argument. Nothing here reads a clock, waits or blocks: a device
//! that never answers (M2-104 Appendix I: a device that does not know UMP
//! Stream messages, or an OS that never delivers them) costs three sends and
//! then is a [`Basis::Unanswered`] device, described by the OS.
//!
//! # The session as an Endpoint
//!
//! While the session presents an identity ([`MidiSession::present`](crate::MidiSession::present)),
//! the device of a UMP Endpoint with an output may ask the session what it is
//! as well: its Endpoint Discovery, Function Block Discovery and Stream
//! Configuration Request are answered on the device's output as the Endpoint
//! the identity describes ([`Endpoint::local`]), by [`Endpoint::reply`].
//!
//! # Identity Request
//!
//! A MIDI 1.0 port with an output has no UMP Stream to ask what it is
//! (M2-104 Appendix I), and is `Described` from the connect all the same. What
//! it can say of itself is its Identity Reply (M1 p.40), so the first poll also
//! sends it an Identity Request to every device (`F0 7E 7F 06 01 F7`, one SysEx7
//! packet), through `tutti_midi_device::IdentityInquiry`: sent again once if it
//! is not answered in
//! [`INQUIRY_TIMEOUT`](tutti_midi_device::INQUIRY_TIMEOUT), then given up on, and
//! a device that never answers is the same `Described` device as before. The
//! reply is read from a copy of the port's SysEx7 packets that the input edge
//! keeps while the request is out
//! ([`HardwareMidiInputs::hear_sysex`](crate::HardwareMidiInputs::hear_sysex));
//! the packets still reach the graph. An answer is returned by
//! [`MidiSession::poll`](crate::MidiSession::poll) as a [`DeviceStream`]
//! of basis [`Basis::Inquired`], whose Endpoint carries the identity.
//!
//! # What is asked for
//!
//! The MIDI 2.0 Protocol where the device offers it, else MIDI 1.0
//! ([`EndpointDiscovery::with_wanted`] narrows the request to what the Endpoint
//! supports), with **JR Timestamps off to the device** (RXJR is never asked
//! for) and, from the device, off unless the input was opened with
//! [`InputTiming::JitterReduction`]. Input time then comes from the OS's
//! arrival stamp (`InputProducerHandle::push`), and the input edge ignores a JR
//! Clock or JR Timestamp a device sends anyway; with
//! [`JitterReduction`](InputTiming::JitterReduction) TXJR is asked for, and a
//! device that settles on it has its events placed by its JR Timestamps. JR on
//! the output side is the session's output stamping, which follows the
//! configuration the negotiation settled (`rx_jr`), so a device that is not
//! asked is not sent it, and one that turns it on itself is.
//!
//! # After the settle
//!
//! A device that answered goes on being heard: a Stream Configuration
//! Notification moves the Protocol and JR in use, and (when the Endpoint did
//! not declare its Function Blocks static) a Function Block Info or Name
//! changes a block (`EndpointDiscovery::on_message`, `DiscoveryAction::Changed`). A change is
//! returned by [`MidiSession::poll`](crate::MidiSession::poll) as the settle
//! is, and what the output sends in follows it, and where the input's event
//! times come from follows its TXJR.

use std::sync::Arc;
use tutti_platform::Instant;

use tutti_graph::{Losses, Offset};
use tutti_midi_ci::ResourceStore;
use tutti_midi_device::{
    Capabilities, DiscoveryAction, DiscoveryEnding, Endpoint, EndpointDiscovery, EndpointReply,
    IdentityInquiry, InquiryAction, Learned, Supports,
};
use tutti_midi_protocol::{Addressed, Universal, UniversalRead, read};
use tutti_midi_runtime::Outgoing;
use tutti_rt::CounterWriter;
use tutti_ump::{
    BlockFilter, BlockSelect, DeviceIdentity, DiscoveryFilter, Message, MidiGroup, Protocol,
    Stream, StreamConfig, SysexAssembler, SysexStep, Ump,
};

use crate::answer::Answer;
use crate::capability::{described, is_ump_endpoint};
use crate::ci::{
    Ci, CiChange, CiChanges, CiPeerInfo, Heard, PeerId, groups_after_settle, groups_of_a_midi1_port,
};
use crate::devices::OpenOutput;
use crate::peer::PeerRequestError;
use crate::process::{self, ProcessChange, ReportAsk};
use crate::profiles::{ProfileError, ProfileSwitch, ProfilesChange, Snapshot};
use crate::properties::{self, PropertyAsk, PropertyChange};
use crate::session::Presented;
use tutti_midi_device::MidiDeviceId;

/// Where an input's event times come from: what a device is asked to send, and
/// what the input edge reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
#[repr(u8)]
pub enum InputTiming {
    /// The OS's arrival stamp of each event. The device is asked for no JR
    /// Timestamps, and one that sends them anyway has them ignored.
    #[default]
    Arrival = 0,
    /// The device's JR Timestamps (M2-104 §7.2.2), where its configuration says
    /// it sends them: it is asked to (TXJR), and a device that agrees has each
    /// event placed at the time its timestamp maps to on the host's clock. A
    /// device that does not agree is timed by arrival, as [`Arrival`] is.
    ///
    /// [`Arrival`]: Self::Arrival
    JitterReduction = 1,
}

impl InputTiming {
    /// The timing as a byte a port holds in an atomic: its discriminant.
    pub(crate) const fn to_raw(self) -> u8 {
        self as u8
    }

    /// The timing a byte holds, `None` for a byte [`to_raw`](Self::to_raw)
    /// never made.
    pub(crate) const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Arrival),
            1 => Some(Self::JitterReduction),
            _ => None,
        }
    }

    /// The configuration the session asks every Endpoint for: the MIDI 2.0
    /// Protocol where it is offered, JR Timestamps only as this asks for them
    /// and never to the device (RXJR).
    const fn wanted(self) -> StreamConfig {
        StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: false,
            tx_jr: matches!(self, Self::JitterReduction),
        }
    }

    /// Where the events of a device configured as `config` are timed from.
    pub(crate) const fn applied(self, config: StreamConfig) -> Self {
        if matches!(self, Self::JitterReduction) && config.tx_jr {
            Self::JitterReduction
        } else {
            Self::Arrival
        }
    }
}

/// How a device's [`DeviceStream`] came to be known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Basis {
    /// The device answered: the Endpoint is what it said, and the
    /// configuration is the last Stream Configuration Notification it sent
    /// (§7.1.6.1 step 3: the peer may refuse any part of a request).
    Negotiated,
    /// The device was asked and did not answer in time. The Endpoint is the
    /// OS's description of it, which for a device an OS has already
    /// discovered (the ALSA kernel does, for USB) is the true one.
    Unanswered,
    /// It was not asked what it is: a device with no output has no way to ask,
    /// and a MIDI 1.0 port that has one has no UMP Stream to ask but is sent an
    /// Identity Request, which it may never answer. The Endpoint is the OS's
    /// description.
    Described,
    /// A MIDI 1.0 port that answered an Identity Request (M1 p.40): the
    /// Endpoint is the OS's description with the identity the device gave
    /// ([`Endpoint::identity`]).
    Inquired,
}

/// What is known of the UMP Stream state of one device: its Endpoint, the
/// Protocol and JR Timestamps in use, and how that came to be known.
///
/// The output side follows [`config`](Self::config): the device's output is
/// sent in the Protocol it names, with JR Timestamps when it receives them
/// (`OpenOutput`'s wire form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceStream {
    endpoint: Endpoint,
    basis: Basis,
}

impl DeviceStream {
    /// The Endpoint: what the device declared, or what the OS says of it.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How it came to be known.
    #[must_use]
    pub const fn basis(&self) -> Basis {
        self.basis
    }

    /// Returns what the device itself said of itself: the Device Identity and
    /// Product Instance Id of a device that [`Negotiated`](Basis::Negotiated),
    /// the identity of one that was [`Inquired`](Basis::Inquired). The OS's
    /// description of an [`Unanswered`](Basis::Unanswered) or
    /// [`Described`](Basis::Described) one carries neither (only a device's own
    /// message sets them), so it says nothing.
    #[must_use]
    pub fn said(&self) -> Learned {
        let learned = self
            .endpoint
            .identity()
            .map_or_else(Learned::new, |identity| {
                Learned::new().identified(*identity)
            });
        match self.endpoint.instance_id() {
            Some(id) => learned.instanced(*id),
            None => learned,
        }
    }

    /// The Protocol and JR Timestamps in use.
    #[must_use]
    pub const fn config(&self) -> StreamConfig {
        self.endpoint.active()
    }
}

/// One device's negotiation: its [`EndpointDiscovery`], while one runs, and the output
/// it sends on. Control side.
pub(crate) struct Negotiation {
    os: Endpoint,
    out: Option<OpenOutput>,
    run: Run,
    inquire: Inquire,
    ci: Ci,
    /// The Endpoint the session answers a peer's Stream messages as, with what
    /// it was built from.
    answering: Option<Answering>,
    timing: InputTiming,
    /// Where the request to stop JR Timestamps the input did not ask for is.
    stop: StopJr,
}

/// The Endpoint the session answers a device's Stream messages as.
struct Answering {
    /// What the Endpoint was built from.
    presented: Presented,
    endpoint: Endpoint,
    /// Whether the device has been sent a Stream Configuration Notification, so
    /// a later change of the Protocol in force is told to it (§7.1.6.3).
    told: bool,
}

/// What a rebuilt answering Endpoint tells a peer that discovered the old one.
struct Announce {
    /// The Function Block's Groups changed: its Info is sent again.
    blocks: bool,
}

const NO_FILTER: DiscoveryFilter = DiscoveryFilter {
    endpoint_info: false,
    device_identity: false,
    endpoint_name: false,
    product_instance_id: false,
    stream_configuration: false,
};

const ALL_FILTER: DiscoveryFilter = DiscoveryFilter {
    endpoint_info: true,
    device_identity: true,
    endpoint_name: true,
    product_instance_id: true,
    stream_configuration: true,
};

/// An Endpoint Discovery for `filter`, answered by `endpoint` into `out`: the
/// unsolicited notifications of what the filter names.
fn ask(endpoint: &mut Endpoint, filter: DiscoveryFilter, out: &mut Vec<Ump>) {
    let _ = endpoint.reply(
        Message::Stream(Stream::EndpointDiscovery {
            version: endpoint.version(),
            filter,
        }),
        out,
    );
}

/// A configuration seen from the other end of the link: the device's `rx_jr`
/// is what the session sends, so it is the session Endpoint's `tx_jr`, and the
/// reverse. The Protocol is the link's.
const fn mirrored(config: StreamConfig) -> StreamConfig {
    StreamConfig {
        protocol: config.protocol,
        rx_jr: config.tx_jr,
        tx_jr: config.rx_jr,
    }
}

/// The request that stops JR Timestamps a device sends the input did not ask
/// for (M2-104 §7.2.2.3): one per episode, an episode being the time the
/// device's TXJR stays on against an input that wants it off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopJr {
    /// TXJR is off, or wanted: nothing to ask.
    Settled,
    /// TXJR is on and unwanted, and the request is not out yet: the output
    /// refused it, and the next poll tries again.
    Unsent,
    /// The request is out. A device that kept TXJR on is not asked again
    /// until the episode ends.
    Asked,
}

impl StopJr {
    /// The state after a poll, where `unwanted` says the device's TXJR is on
    /// against the input, and `send` queues the request: whether the output
    /// took it. A request is made once per episode and retried each poll
    /// until it goes out. `send` is told whether this is a retry, which is
    /// to be quiet: a refusal is counted and logged once per episode.
    fn next(self, unwanted: bool, send: impl FnOnce(bool) -> bool) -> Self {
        match (unwanted, self) {
            (false, _) => Self::Settled,
            (true, Self::Asked) => Self::Asked,
            (true, state) if send(state == Self::Unsent) => Self::Asked,
            (true, _) => Self::Unsent,
        }
    }
}

/// What one [`Negotiation::poll`] did.
pub(crate) struct Stepped {
    /// The stream, when this call settled it or changed it.
    pub(crate) stream: Option<DeviceStream>,
    /// The MIDI-CI peers that came, changed or went.
    pub(crate) ci: Vec<(PeerId, CiChange)>,
    /// The Profiles of the peers that changed.
    pub(crate) profiles: Vec<(PeerId, ProfilesChange)>,
    /// The Process Inquiry of the peers that changed.
    pub(crate) process: Vec<(PeerId, ProcessChange)>,
    /// The Property Exchange of the peers that changed.
    pub(crate) properties: Vec<(PeerId, PropertyChange)>,
    /// How the requests asked of peers ended.
    pub(crate) answers: Vec<(PeerId, Answer)>,
    /// The Groups whose MIDI-CI device could not start, with why.
    pub(crate) unstarted: Vec<(MidiGroup, getrandom::Error)>,
}

/// The Identity Request of a MIDI 1.0 port that has an output (the module's
/// "Identity Request"). It runs beside a `Run::Done` that is `Described` from
/// the connect, so a MIDI 1.0 port is settled at once and the inquiry only
/// adds to it.
enum Inquire {
    /// Nothing to ask: a UMP Endpoint (it has a `Run`), a device with no
    /// output, or an inquiry that has ended.
    No,
    /// Decided to ask, not yet started: the first poll sends the request.
    Idle,
    Asking(Box<Asking>),
}

/// The request out, and what hears the reply.
struct Asking {
    inquiry: IdentityInquiry,
    assembler: SysexAssembler<Box<[u8]>>,
}

/// The Group of a MIDI 1.0 port's UMPs.
const LEGACY_GROUP: MidiGroup = MidiGroup::of(0);

/// The longest message the reply reader takes in, `F0` and `F7` included: an
/// Identity Reply is 17 bytes with a three-byte manufacturer id.
const REPLY_ROOM: usize = 32;

impl Asking {
    fn new(now: Instant) -> (Self, InquiryAction) {
        let (inquiry, first) = IdentityInquiry::new(LEGACY_GROUP, now);
        let room = SysexAssembler::room_for(REPLY_ROOM, 1, 1);
        let assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), REPLY_ROOM, 1);
        (Self { inquiry, assembler }, first)
    }

    /// Takes in one SysEx7 packet: the identity, if it completed an Identity
    /// Reply. The reply's device id is the responder's own and is not looked
    /// at: the request went to every device (`7F`) of one port.
    fn hear(&mut self, ump: Ump) -> Option<DeviceIdentity> {
        self.assembler.begin_block();
        let SysexStep::Complete(done) = self.assembler.push(ump) else {
            return None;
        };
        match read(self.assembler.message(done)) {
            UniversalRead::Universal(Addressed {
                message: Universal::IdentityReply(identity),
                ..
            }) => Some(identity),
            _ => None,
        }
    }
}

enum Run {
    /// Decided to ask, not yet started: the first poll starts it.
    Idle,
    Asking(Box<EndpointDiscovery>),
    /// Settled. `listening` is the connection of a device that answered, which
    /// still hears what it changes; a device described by the OS has none.
    Done {
        stream: Box<DeviceStream>,
        listening: Option<Box<EndpointDiscovery>>,
    },
}

impl Negotiation {
    /// The negotiation for a device the OS describes as `os`, on `out` when it
    /// has an output. It asks only a UMP Endpoint that has an output; any
    /// other is [`Basis::Described`] at once.
    pub(crate) fn new(
        device: MidiDeviceId,
        os: Endpoint,
        out: Option<OpenOutput>,
        timing: InputTiming,
        ci_refused: CounterWriter<Losses>,
    ) -> Self {
        let asks = is_ump_endpoint(&os) && out.is_some();
        let inquire = if !is_ump_endpoint(&os) && out.is_some() {
            Inquire::Idle
        } else {
            Inquire::No
        };
        let run = if asks {
            Run::Idle
        } else {
            Run::Done {
                stream: Box::new(DeviceStream {
                    endpoint: described(&os),
                    basis: Basis::Described,
                }),
                listening: None,
            }
        };
        Self {
            os,
            out,
            run,
            inquire,
            ci: Ci::new(device, ci_refused),
            answering: None,
            timing,
            stop: StopJr::Settled,
        }
    }

    /// What is known, once it is settled.
    pub(crate) fn settled(&self) -> Option<DeviceStream> {
        match &self.run {
            Run::Done { stream, .. } => Some(**stream),
            _ => None,
        }
    }

    /// Whether the port should copy the SysEx7 packets the device sends to the
    /// stream ring: an Identity Request is about to go out or is out, or the
    /// session has a MIDI-CI identity (`has_identity`) and its devices run or
    /// are about to.
    pub(crate) fn hears_sysex(&self, has_identity: bool) -> bool {
        matches!(self.inquire, Inquire::Idle | Inquire::Asking(_))
            || self.ci.hears_sysex(has_identity)
    }

    /// The input is closing: MIDI-CI ends on the device's output (see
    /// [`Ci::close`]).
    pub(crate) fn close(&mut self) {
        if let Some(out) = &mut self.out {
            self.ci.close(out);
        }
    }

    /// The MIDI-CI peers found on this device.
    pub(crate) fn ci_peers(&self) -> Arc<[CiPeerInfo]> {
        self.ci.peers()
    }

    /// The Profiles of the MIDI-CI peers found on this device.
    pub(crate) fn profiles(&self) -> Snapshot {
        self.ci.profiles()
    }

    /// The MIDI-CI peers of this device that can be asked for a MIDI Message
    /// Report.
    pub(crate) fn report_peers(&self) -> process::Snapshot {
        self.ci.report_peers()
    }

    /// The MIDI-CI peers of this device that can be asked for Property Exchange.
    pub(crate) fn property_peers(&self) -> properties::Snapshot {
        self.ci.property_peers()
    }

    /// Asks that a peer be made a Property Exchange call at the next poll.
    pub(crate) fn ask_property(&mut self, ask: PropertyAsk) -> Result<(), PeerRequestError> {
        self.ci.ask_property(ask)
    }

    /// Asks that a peer be sent a MIDI Message Report request at the next poll.
    pub(crate) fn report(&mut self, ask: ReportAsk) -> Result<(), PeerRequestError> {
        self.ci.report(ask)
    }

    /// Asks that a peer's Profile be switched at the next poll.
    pub(crate) fn set_profile(&mut self, switch: ProfileSwitch) -> Result<(), ProfileError> {
        self.ci.set_profile(switch)
    }

    /// The Identity Request of a MIDI 1.0 port: starts it, or hands it the
    /// packets the port copied (`incoming`) and the time. Returns the stream
    /// when the device answered, with the identity it gave.
    fn poll_inquiry(&mut self, now: Instant, heard: &Heard<'_>) -> Option<DeviceStream> {
        let mut actions = Vec::new();
        match &mut self.inquire {
            Inquire::No => {}
            Inquire::Idle => {
                let (asking, first) = Asking::new(now);
                actions.push(first);
                self.inquire = Inquire::Asking(Box::new(asking));
            }
            Inquire::Asking(asking) => {
                for &ump in heard.sysex() {
                    if let Some(identity) = asking.hear(ump) {
                        actions.push(asking.inquiry.on_reply(identity));
                    }
                }
                if heard.gap() {
                    // A reply cut by a gap is not completed by what follows it.
                    asking.assembler.lost_tail();
                }
                if asking.inquiry.deadline().is_some_and(|at| now >= at) {
                    actions.push(asking.inquiry.on_timeout(now));
                }
            }
        }
        for action in actions {
            match action {
                InquiryAction::Wait => {}
                InquiryAction::Send(ump) => self.send(ump),
                InquiryAction::GaveUp => {
                    self.inquire = Inquire::No;
                    self.ci.decide(groups_of_a_midi1_port());
                }
                InquiryAction::Answered(identity) => {
                    self.inquire = Inquire::No;
                    self.ci.decide(groups_of_a_midi1_port());
                    let Run::Done { stream, .. } = &mut self.run else {
                        return None;
                    };
                    stream.endpoint = stream.endpoint.identified(identity);
                    stream.basis = Basis::Inquired;
                    return Some(**stream);
                }
            }
        }
        None
    }

    /// Takes what the port heard (`heard`: the device's Stream messages and,
    /// while MIDI-CI or the Identity Request listens, its SysEx7 packets) and
    /// the time `now`: starts the negotiation, or steps it, and steps MIDI-CI.
    /// Returns the stream when this call settled it, or, after that, changed
    /// it, and the peers' changes.
    ///
    /// `presented` is who the session is on the wire, if it presents anyone: its
    /// identity is what MIDI-CI runs with, and its Endpoint answers what the
    /// device asks of it ([`answer`](Self::answer)).
    ///
    /// MIDI-CI steps first: what a settle or an inquiry's end makes it start is
    /// started by the next poll, after the port has been asked to copy SysEx.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        heard: &Heard<'_>,
        presented: Option<Presented>,
        store: &mut ResourceStore,
    ) -> Stepped {
        let ci = presented.map(|p| p.identity.device);
        let changes = match &mut self.out {
            Some(out) => self.ci.poll(now, heard, ci, store, out),
            None => CiChanges::default(),
        };
        let stream = self.poll_stream(now, heard);
        let granted = self.answer(heard, presented);
        Stepped {
            stream: granted.or(stream),
            ci: changes.peers,
            profiles: changes.profiles,
            process: changes.process,
            properties: changes.properties,
            answers: changes.answers,
            unstarted: changes.unstarted,
        }
    }

    /// Answers what the device asked of the session as an Endpoint (M2-104
    /// §7.1): Endpoint Discovery, Function Block Discovery and a Stream
    /// Configuration Request, each on the device's output. Only a UMP Endpoint
    /// with an output is answered, and only while the session presents an
    /// identity. Returns the device's stream when a granted request moved it.
    ///
    /// The Protocol in force is one state for both directions: the settled
    /// [`DeviceStream`]'s. The answering Endpoint reports it (and a change of
    /// it, once the session has told the device its configuration), and a
    /// request granted here moves it, so the output follows what the
    /// Notification said, Protocol and JR Timestamps both (the device's TXJR is
    /// the Endpoint's RXJR). A request is granted within what the Endpoint
    /// declares ([`supports`](Self::supports)); JR Timestamps asked off are
    /// turned off, and the input is then timed by arrival. The session itself asks
    /// only during its own negotiation, before the stream settles, and
    /// afterwards only to turn off JR Timestamps the input did not ask for,
    /// naming the Protocol in force; so answering a request starts none, and a
    /// later request of the session's cannot undo a grant.
    fn answer(&mut self, heard: &Heard<'_>, presented: Option<Presented>) -> Option<DeviceStream> {
        let announce = self.rebuild(presented);
        let (Some(answering), true) = (&mut self.answering, is_ump_endpoint(&self.os)) else {
            return None;
        };
        let mut replies: Vec<Ump> = Vec::new();
        if let Run::Done { stream, .. } = &self.run {
            let in_force = mirrored(stream.endpoint.active());
            if answering.endpoint.active() != in_force {
                answering.endpoint = answering.endpoint.in_use(in_force);
                if answering.told {
                    ask(
                        &mut answering.endpoint,
                        DiscoveryFilter {
                            stream_configuration: true,
                            ..NO_FILTER
                        },
                        &mut replies,
                    );
                }
            }
        }
        if let Some(announce) = announce {
            ask(
                &mut answering.endpoint,
                DiscoveryFilter {
                    stream_configuration: false,
                    ..ALL_FILTER
                },
                &mut replies,
            );
            if announce.blocks {
                let _ = answering.endpoint.reply(
                    Message::Stream(Stream::FunctionBlockDiscovery {
                        block: BlockSelect::All,
                        filter: BlockFilter {
                            info: true,
                            name: false,
                        },
                    }),
                    &mut replies,
                );
            }
        }
        let mut granted = None;
        for &ump in heard.packets {
            if let Ok(message @ Message::Stream(stream)) = ump.decode()
                && matches!(
                    stream,
                    Stream::EndpointDiscovery { .. }
                        | Stream::FunctionBlockDiscovery { .. }
                        | Stream::StreamConfigurationRequest(_)
                )
                && let EndpointReply::Configured(configuration) =
                    answering.endpoint.reply(message, &mut replies)
            {
                granted = Some(configuration.active());
            }
        }
        answering.told |= replies.iter().any(|ump| {
            matches!(
                ump.decode(),
                Ok(Message::Stream(Stream::StreamConfigurationNotification(_)))
            )
        });
        for ump in replies {
            queue(&mut self.out, ump, false);
        }
        let active = granted?;
        let Run::Done { stream, listening } = &mut self.run else {
            return None;
        };
        let config = mirrored(active);
        if stream.endpoint.active() == config {
            return None;
        }
        stream.endpoint = stream.endpoint.in_use(config);
        if let Some(connection) = listening {
            connection.adopt(config);
        }
        Some(**stream)
    }

    /// What the answering Endpoint declares it supports: both Protocols; JR
    /// Timestamps received when the input asks the device for them
    /// ([`InputTiming::JitterReduction`]), so a peer's request can keep them on;
    /// and JR Timestamps sent, since the session's output JR-stamps for a device
    /// whose configuration has it receiving them.
    fn supports(&self) -> Supports {
        Supports::of(Capabilities {
            midi1: true,
            midi2: true,
            rx_jr: matches!(self.timing, InputTiming::JitterReduction),
            tx_jr: true,
        })
    }

    /// Makes the answering Endpoint the one `presented` describes: built when
    /// the session starts presenting, kept while it does not change, rebuilt
    /// when it does (the Protocol in use kept, or the settled stream's when it
    /// is built after the settle). What a rebuild changes for a peer that
    /// discovered the old one is returned: the Endpoint is announced again, and
    /// its Function Block when the Groups changed.
    fn rebuild(&mut self, presented: Option<Presented>) -> Option<Announce> {
        let Some(now) = presented else {
            self.answering = None;
            return None;
        };
        if self.answering.as_ref().is_some_and(|a| a.presented == now) {
            return None;
        }
        let was = self.answering.take();
        let mut endpoint = Endpoint::local(self.supports(), now.groups)
            .identified(now.identity.device)
            .named(now.identity.name);
        if let Some(id) = now.identity.instance {
            endpoint = endpoint.instanced(id);
        }
        if let Some(was) = &was {
            endpoint = endpoint.in_use(was.endpoint.active());
        } else if let Run::Done { stream, .. } = &self.run {
            endpoint = endpoint.in_use(mirrored(stream.endpoint.active()));
        }
        let told = was.as_ref().is_some_and(|a| a.told);
        let announce = was.as_ref().map(|a| Announce {
            blocks: a.presented.groups != now.groups,
        });
        self.answering = Some(Answering {
            presented: now,
            endpoint,
            told,
        });
        announce
    }

    fn poll_stream(&mut self, now: Instant, heard: &Heard<'_>) -> Option<DeviceStream> {
        let incoming = heard.packets;
        if !matches!(self.inquire, Inquire::No) {
            return self.poll_inquiry(now, heard);
        }
        let mut actions = Vec::new();
        let wanted = self.timing.wanted();
        match &mut self.run {
            Run::Idle => {
                let (connection, first) = EndpointDiscovery::with_wanted(wanted, now);
                actions.push(first);
                self.run = Run::Asking(Box::new(connection));
            }
            Run::Asking(connection) => {
                for &ump in incoming {
                    if let Ok(message) = ump.decode() {
                        actions.push(connection.on_message(message, now));
                    }
                }
                if connection.deadline().is_some_and(|at| now >= at) {
                    actions.push(connection.on_timeout(now));
                }
            }
            Run::Done { stream, listening } => {
                let connection = listening.as_mut()?;
                let mut changed = false;
                for &ump in incoming {
                    if let Ok(message) = ump.decode() {
                        changed |= connection.on_message(message, now) == DiscoveryAction::Changed;
                    }
                }
                let mut reported = None;
                if changed {
                    stream.endpoint = *connection.endpoint();
                    // MIDI-CI follows: its Groups are the blocks' now, taken
                    // up by the next poll (`Ci::poll`).
                    self.ci.decide(groups_after_settle(stream));
                    reported = Some(**stream);
                }
                // A device that sends JR Timestamps the input did not ask for
                // is asked once to stop (M2-104 §7.2.2.3), and again only
                // after its TXJR has gone off and come back on.
                let unwanted = stream.endpoint.active().tx_jr && !wanted.tx_jr;
                // The Protocol in force is kept: the request is to stop JR
                // Timestamps, not to move a Protocol a peer's request chose.
                let asked = StreamConfig {
                    tx_jr: wanted.tx_jr,
                    ..stream.endpoint.active()
                };
                let out = &mut self.out;
                self.stop = self.stop.next(unwanted, |retry| {
                    let request = Stream::StreamConfigurationRequest(asked);
                    queue(out, Ump::from(Message::Stream(request)), retry)
                });
                return reported;
            }
        }
        for action in actions {
            match action {
                DiscoveryAction::Wait | DiscoveryAction::Changed => {}
                DiscoveryAction::Send(ump) => self.send(ump),
                DiscoveryAction::Ended(ending) => return self.settle(ending),
            }
        }
        None
    }

    /// Sends a negotiation step. One the output refused is counted by the
    /// output ([`OpenOutput::losses`]) and sent again when the step times out.
    fn send(&mut self, ump: Ump) {
        queue(&mut self.out, ump, false);
    }

    fn settle(&mut self, ending: DiscoveryEnding) -> Option<DeviceStream> {
        let Run::Asking(connection) = &self.run else {
            return None;
        };
        let (stream, listening) = match ending {
            DiscoveryEnding::Negotiated => (
                DeviceStream {
                    endpoint: *connection.endpoint(),
                    basis: Basis::Negotiated,
                },
                Some(connection.clone()),
            ),
            DiscoveryEnding::Midi1Fallback => (
                DeviceStream {
                    endpoint: described(&self.os),
                    basis: Basis::Unanswered,
                },
                None,
            ),
        };
        self.ci.decide(groups_after_settle(&stream));
        self.run = Run::Done {
            stream: Box::new(stream),
            listening,
        };
        Some(stream)
    }
}

/// Queues `ump` to the device: whether the output took it. A refusal is
/// counted by the output and logged, unless `retry` says it is a repeat of an
/// offer that was refused already.
fn queue(out: &mut Option<OpenOutput>, ump: Ump, retry: bool) -> bool {
    let Some(out) = out else { return false };
    let events = Outgoing::now_batch(&[(Offset::ZERO, ump)]);
    let (taken, refused) = out.offer(&events);
    if refused > 0 && !retry {
        out.count_refused(refused);
        tracing::debug!("a UMP Stream message was refused by {}", out.name());
    }
    !taken.is_none()
}

#[cfg(test)]
mod tests;
