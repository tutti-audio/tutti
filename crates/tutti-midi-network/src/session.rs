//! One endpoint's relationship with one peer: the session state machine of
//! M2-124 section 6, over no socket and no clock of its own.
//!
//! [`Session<R>`] holds what both roles share: the link, ping and liveness, the
//! established, resetting and closing life, the queued commands and events. A
//! [`Role`] holds what only one role has: its data, the states before a Session
//! is established and the commands addressed to it.

mod client;
mod host;

use std::collections::VecDeque;
use std::net::SocketAddr;
use tutti_platform::Instant;

use tutti_ump::Ump;
use tutti_units::Losses;

use crate::command::code;
use crate::link::{Link, Loss};
use crate::{
    AuthChallenge, AuthFailure, Bye, ByeReason, Command, Digest, EndedBy, EndpointIdentity, Event,
    Header, Invitation, Nak, NakReason, PingId, Retransmit, RetransmitRefusal, RetransmitRequest,
    SendError, SessionConfig, SessionState, UmpData, UserAuthChallenge, UserAuthentication,
};

pub(crate) use client::ClientRole;
pub(crate) use host::{Capacity, HostRole};

/// A command sent again until it is answered or `command_timeout` passes.
#[derive(Clone, Debug)]
pub(crate) struct Repeat {
    command: Command,
    started: Instant,
    next: Instant,
}

impl Repeat {
    fn deadline(&self, cfg: &SessionConfig) -> Instant {
        self.next.min(self.started + cfg.command_timeout.get())
    }

    /// Sends the command again when its repeat is due.
    fn repeat_due(&mut self, now: Instant, cfg: &SessionConfig, out: &mut Vec<Command>) {
        if now >= self.next {
            self.next = now + cfg.repeat_interval.get();
            out.push(self.command.clone());
        }
    }
}

/// Where a Session stands: the states of its life, and the role's own states
/// before it is established.
#[derive(Clone, Debug)]
enum Phase<Opening> {
    Idle,
    /// A state only the role has, between the first Invitation and the Session.
    Opening(Opening),
    Established,
    Resetting(Repeat),
    Closing {
        repeat: Repeat,
        reason: ByeReason,
    },
}

/// What one of the two roles has that the other has not.
///
/// A command the role is not the addressee of is answered with a NAK, Command
/// Not Expected (M2-124 sections 6.4 to 6.10).
pub(crate) trait Role: Sized {
    /// The states before the Session is established.
    type Opening: std::fmt::Debug;

    fn opening_state(opening: &Self::Opening) -> SessionState;

    fn opening_deadline(opening: &Self::Opening, cfg: &SessionConfig) -> Instant;

    /// Does the work due at `now` in an opening state.
    fn tick_opening(&mut self, core: &mut Core<Self>, now: Instant);

    fn on_invitation(
        &mut self,
        core: &mut Core<Self>,
        _now: Instant,
        header: Header,
        _invitation: Invitation,
    ) {
        core.nak(NakReason::NotExpected, header);
    }

    fn on_auth(&mut self, core: &mut Core<Self>, _now: Instant, header: Header, _digest: &Digest) {
        core.nak(NakReason::NotExpected, header);
    }

    fn on_user_auth(
        &mut self,
        core: &mut Core<Self>,
        _now: Instant,
        header: Header,
        _auth: &UserAuthentication,
    ) {
        core.nak(NakReason::NotExpected, header);
    }

    fn on_accepted(
        &mut self,
        core: &mut Core<Self>,
        _now: Instant,
        header: Header,
        _identity: EndpointIdentity,
    ) {
        core.nak(NakReason::NotExpected, header);
    }

    fn on_pending(
        &mut self,
        core: &mut Core<Self>,
        _now: Instant,
        header: Header,
        _identity: EndpointIdentity,
    ) {
        core.nak(NakReason::NotExpected, header);
    }

    fn on_challenge(
        &mut self,
        core: &mut Core<Self>,
        _now: Instant,
        header: Header,
        _challenge: &AuthChallenge,
    ) {
        core.nak(NakReason::NotExpected, header);
    }

    fn on_user_challenge(
        &mut self,
        core: &mut Core<Self>,
        _now: Instant,
        header: Header,
        _challenge: &UserAuthChallenge,
    ) {
        core.nak(NakReason::NotExpected, header);
    }
}

/// What both roles of a Session hold.
#[derive(Debug)]
pub(crate) struct Core<R: Role> {
    cfg: SessionConfig,
    local: EndpointIdentity,
    peer: SocketAddr,
    phase: Phase<R::Opening>,
    remote: Option<EndpointIdentity>,
    link: Link,
    last_heard: Instant,
    last_ping: Option<Instant>,
    ping_id: PingId,
    outstanding_pings: VecDeque<PingId>,
    out: Vec<Command>,
    events: Vec<Event>,
    /// Commands whose advisory text a peer sent partly unreadable.
    losses: Losses,
}

/// A Session in one role.
#[derive(Debug)]
pub(crate) struct Session<R: Role> {
    core: Core<R>,
    role: R,
}

/// The most Pings that may be unanswered at once before the oldest is dropped.
const OUTSTANDING_PINGS: usize = 8;

impl<R: Role> Session<R> {
    fn new(
        role: R,
        cfg: SessionConfig,
        local: EndpointIdentity,
        peer: SocketAddr,
        now: Instant,
    ) -> Self {
        Self {
            core: Core {
                cfg,
                local,
                peer,
                phase: Phase::Idle,
                remote: None,
                link: Link::new(),
                last_heard: now,
                last_ping: None,
                ping_id: PingId::new(0),
                outstanding_pings: VecDeque::new(),
                out: Vec::new(),
                events: Vec::new(),
                losses: Losses::NONE,
            },
            role,
        }
    }

    pub(crate) fn state(&self) -> SessionState {
        self.core.state()
    }

    /// The commands and events the last calls produced, and what they lost.
    pub(crate) fn take_output(&mut self) -> (Vec<Command>, Vec<Event>, Losses) {
        let core = &mut self.core;
        (
            std::mem::take(&mut core.out),
            std::mem::take(&mut core.events),
            std::mem::take(&mut core.losses),
        )
    }

    pub(crate) fn remote(&self) -> Option<EndpointIdentity> {
        self.core.remote
    }

    /// Ends the Session or the invitation with a Bye that is repeated until
    /// answered.
    pub(crate) fn close(&mut self, now: Instant, reason: ByeReason) {
        self.core.close(now, reason);
    }

    /// Starts a Session Reset (M2-124 section 6.11).
    pub(crate) fn reset(&mut self, now: Instant) {
        self.core.reset(now);
    }

    /// Queues UMP for the peer.
    pub(crate) fn send_ump(&mut self, now: Instant, packets: &[Ump]) -> Result<(), SendError> {
        let core = &mut self.core;
        if !matches!(core.phase, Phase::Established) {
            return Err(SendError::NotEstablished {
                state: core.state(),
            });
        }
        core.link.send(now, &core.cfg, packets, &mut core.out);
        Ok(())
    }

    /// Takes one command of a received datagram, with the header it arrived
    /// under: a NAK of the command quotes that header.
    pub(crate) fn on_message(&mut self, header: Header, command: Command, now: Instant) {
        let (core, role) = (&mut self.core, &mut self.role);
        core.last_heard = now;
        match command {
            Command::Ping(id) => core.out.push(Command::PingReply(id)),
            Command::PingReply(id) => core.on_ping_reply(id, header),
            Command::Nak(nak) => core.on_nak(now, &nak),
            Command::Bye(bye) => core.on_bye(&bye),
            Command::ByeReply => {
                if let Phase::Closing { reason, .. } = core.phase {
                    core.finish(EndedBy::Local, reason);
                }
            }
            Command::Invitation(inv) => role.on_invitation(core, now, header, inv),
            Command::InvitationWithAuth(digest) => role.on_auth(core, now, header, &digest),
            Command::InvitationWithUserAuth(auth) => role.on_user_auth(core, now, header, &auth),
            Command::Accepted(identity) => role.on_accepted(core, now, header, identity),
            Command::Pending(identity) => role.on_pending(core, now, header, identity),
            Command::AuthRequired(challenge) => role.on_challenge(core, now, header, &challenge),
            Command::UserAuthRequired(challenge) => {
                role.on_user_challenge(core, now, header, &challenge);
            }
            Command::UmpData(data) => core.on_ump_data(now, data),
            Command::RetransmitRequest(request) => core.on_retransmit_request(now, header, request),
            Command::RetransmitError(error) => core.on_retransmit_error(now, error),
            Command::SessionReset => core.on_session_reset(now),
            Command::SessionResetReply => core.on_session_reset_reply(now),
        }
    }

    /// Takes a command that could not be read: unknown codes are NAKed as
    /// unsupported, anything else as malformed. A malformed Invitation is a
    /// protocol error that ends the exchange.
    pub(crate) fn refuse_command(
        &mut self,
        now: Instant,
        header: Header,
        error: crate::ParseError,
    ) {
        let core = &mut self.core;
        core.last_heard = now;
        match error {
            crate::ParseError::UnknownCode(_) => core.nak(NakReason::NotSupported, header),
            crate::ParseError::Truncated => {}
            _ if header.code() == code::INVITATION => {
                if matches!(core.phase, Phase::Idle) {
                    core.refuse(now, ByeReason::ProtocolError);
                }
            }
            _ => core.nak(NakReason::Malformed, header),
        }
    }

    /// The next instant `on_timeout` has work at.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let core = &self.core;
        let cfg = &core.cfg;
        let earliest = |times: &[Option<Instant>]| times.iter().flatten().copied().min();
        match &core.phase {
            Phase::Idle => None,
            Phase::Opening(opening) => Some(R::opening_deadline(opening, cfg)),
            Phase::Resetting(r) | Phase::Closing { repeat: r, .. } => Some(r.deadline(cfg)),
            Phase::Established => earliest(&[
                Some(core.last_heard + cfg.liveness_timeout.get()),
                Some(core.next_ping()),
                core.link.idle_deadline(),
                core.link.rx_deadline(),
            ]),
        }
    }

    pub(crate) fn on_timeout(&mut self, now: Instant) {
        let (core, role) = (&mut self.core, &mut self.role);
        let cfg = core.cfg;
        match &mut core.phase {
            Phase::Idle => {}
            Phase::Opening(_) => role.tick_opening(core, now),
            Phase::Resetting(r) => {
                if now >= r.started + cfg.command_timeout.get() {
                    return core.close(now, ByeReason::Timeout);
                }
                r.repeat_due(now, &cfg, &mut core.out);
            }
            Phase::Closing { repeat, reason } => {
                let reason = *reason;
                if now >= repeat.started + cfg.command_timeout.get() {
                    return core.finish(EndedBy::Local, reason);
                }
                repeat.repeat_due(now, &cfg, &mut core.out);
            }
            Phase::Established => core.tick_established(now),
        }
    }
}

impl<R: Role> Core<R> {
    fn state(&self) -> SessionState {
        match &self.phase {
            Phase::Idle => SessionState::Idle,
            Phase::Opening(opening) => R::opening_state(opening),
            Phase::Established => SessionState::Established,
            Phase::Resetting(_) => SessionState::PendingSessionReset,
            Phase::Closing { .. } => SessionState::PendingBye,
        }
    }

    fn start_repeat(&mut self, now: Instant, command: Command) -> Repeat {
        self.out.push(command.clone());
        Repeat {
            command,
            started: now,
            next: now + self.cfg.repeat_interval.get(),
        }
    }

    fn nak(&mut self, reason: NakReason, refused: Header) {
        self.out.push(Command::Nak(Box::new(Nak {
            reason,
            refused,
            text: crate::NakText::default(),
            text_cut: 0,
        })));
    }

    fn close(&mut self, now: Instant, reason: ByeReason) {
        if matches!(self.phase, Phase::Closing { .. } | Phase::Idle) {
            return;
        }
        self.refuse(now, reason);
    }

    /// Ends the exchange with a peer that was never in a Session: a Bye
    /// repeated until answered, with no state before it.
    fn refuse(&mut self, now: Instant, reason: ByeReason) {
        let repeat = self.start_repeat(now, Command::Bye(Box::new(Bye::new(reason))));
        self.phase = Phase::Closing { repeat, reason };
    }

    fn finish(&mut self, ended_by: EndedBy, reason: ByeReason) {
        self.phase = Phase::Idle;
        self.link = Link::new();
        self.remote = None;
        self.outstanding_pings.clear();
        self.last_ping = None;
        self.events.push(Event::Closed {
            peer: self.peer,
            ended_by,
            reason,
        });
    }

    fn reset(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Established) {
            let repeat = self.start_repeat(now, Command::SessionReset);
            self.phase = Phase::Resetting(repeat);
        }
    }

    /// Begins the Session. A Host's Accepted is queued by the Host.
    fn establish(&mut self, now: Instant) {
        self.link.reset();
        self.last_heard = now;
        self.last_ping = None;
        self.phase = Phase::Established;
        if let Some(remote) = self.remote {
            self.events.push(Event::Established {
                peer: self.peer,
                remote,
            });
        }
    }

    fn not_established(&mut self, now: Instant) {
        if !matches!(self.phase, Phase::Closing { .. }) {
            self.refuse(now, ByeReason::SessionNotEstablished);
        }
    }

    fn report(&mut self, delivered: Vec<Ump>, lost: Vec<Loss>) {
        let peer = self.peer;
        self.events
            .extend(lost.into_iter().map(|l| Event::DataLost {
                peer,
                first: l.first,
                count: l.count,
            }));
        self.events.extend(
            delivered
                .into_iter()
                .map(|packet| Event::Ump { peer, packet }),
        );
    }

    fn on_ping_reply(&mut self, id: PingId, header: Header) {
        if let Some(at) = self.outstanding_pings.iter().position(|&p| p == id) {
            self.outstanding_pings.remove(at);
        } else {
            self.nak(NakReason::BadPingReply, header);
        }
    }

    fn on_ump_data(&mut self, now: Instant, data: UmpData) {
        match self.phase {
            Phase::Established => {
                let mut delivered = Vec::new();
                let mut lost = Vec::new();
                self.link
                    .receive(now, &self.cfg, data, &mut delivered, &mut lost);
                self.report(delivered, lost);
            }
            Phase::Resetting(_) => {}
            _ => self.not_established(now),
        }
    }

    fn on_retransmit_request(&mut self, now: Instant, header: Header, request: RetransmitRequest) {
        match self.phase {
            Phase::Established | Phase::Resetting(_)
                if self.cfg.retransmit == Retransmit::Unsupported =>
            {
                self.nak(NakReason::NotSupported, header);
            }
            Phase::Established => self.link.retransmit(request, &mut self.out),
            Phase::Resetting(_) => {}
            _ => self.not_established(now),
        }
    }

    fn on_retransmit_error(&mut self, now: Instant, error: RetransmitRefusal) {
        match self.phase {
            Phase::Established => {
                let mut delivered = Vec::new();
                let mut lost = Vec::new();
                self.link
                    .on_retransmit_error(error, &mut delivered, &mut lost);
                self.report(delivered, lost);
            }
            Phase::Resetting(_) => {}
            _ => self.not_established(now),
        }
    }

    fn on_session_reset(&mut self, now: Instant) {
        match self.phase {
            Phase::Established | Phase::Resetting(_) => {
                self.out.push(Command::SessionResetReply);
                self.link.reset();
                self.events.push(Event::Reset { peer: self.peer });
            }
            _ => self.not_established(now),
        }
    }

    fn on_session_reset_reply(&mut self, now: Instant) {
        match self.phase {
            Phase::Resetting(_) => {
                self.link.reset();
                self.phase = Phase::Established;
                self.events.push(Event::Reset { peer: self.peer });
            }
            Phase::Established => self.reset(now),
            _ => self.not_established(now),
        }
    }

    fn on_nak(&mut self, now: Instant, nak: &Nak) {
        self.count_cut_text(nak.text_cut);
        if nak.reason == NakReason::NotSupported
            && nak.refused.code() == code::RETRANSMIT_REQUEST
            && matches!(self.phase, Phase::Established)
        {
            self.link.on_retransmit_refused(now);
        }
        self.events.push(Event::Nak {
            peer: self.peer,
            reason: nak.reason,
            refused: nak.refused,
        });
    }

    /// Counts a command whose advisory text a peer sent partly unreadable.
    fn count_cut_text(&mut self, cut: usize) {
        if cut > 0 {
            self.losses += Losses::lost(1);
        }
    }

    fn on_bye(&mut self, bye: &Bye) {
        self.count_cut_text(bye.text_cut);
        // Every Bye is answered, repeated ones and ones from a peer that is
        // in no Session with this endpoint included.
        self.out.push(Command::ByeReply);
        match self.phase {
            Phase::Idle => {}
            Phase::Closing { reason, .. } => self.finish(EndedBy::Local, reason),
            _ => self.finish(EndedBy::Remote, bye.reason),
        }
    }

    fn authentication_failed(&mut self, now: Instant, cause: AuthFailure) {
        self.events.push(Event::AuthenticationFailed {
            peer: self.peer,
            cause,
        });
        self.close(now, ByeReason::InvitationCanceled);
    }

    fn next_ping(&self) -> Instant {
        let quiet_until = self.last_heard + self.cfg.ping_interval.get();
        match self.last_ping {
            Some(sent) => quiet_until.max(sent + self.cfg.ping_interval.get()),
            None => quiet_until,
        }
    }

    fn tick_established(&mut self, now: Instant) {
        let cfg = self.cfg;
        if now >= self.last_heard + cfg.liveness_timeout.get() {
            return self.close(now, ByeReason::Timeout);
        }
        if now >= self.next_ping() {
            let id = self.ping_id;
            self.ping_id = id.next();
            self.outstanding_pings.push_back(id);
            if self.outstanding_pings.len() > OUTSTANDING_PINGS {
                self.outstanding_pings.pop_front();
            }
            self.last_ping = Some(now);
            self.out.push(Command::Ping(id));
        }
        self.link.on_idle(now, &cfg, &mut self.out);
        let mut delivered = Vec::new();
        let mut lost = Vec::new();
        self.link
            .on_rx_timeout(now, &cfg, &mut self.out, &mut delivered, &mut lost);
        self.report(delivered, lost);
    }
}
