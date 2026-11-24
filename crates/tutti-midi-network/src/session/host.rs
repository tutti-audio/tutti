//! The Host role: it answers Invitations, challenges and admits Clients.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;

use super::{Core, Phase, Role, Session};
use crate::challenge::{Because, Challenge, Verdict};
use crate::{
    Admission, ByeReason, Command, Digest, EndpointIdentity, Event, Header, Invitation, NakReason,
    Nonce, SessionConfig, SessionState, UserAuthentication,
};

/// Failed authentications a Host tolerates before it ends the invitation.
const MAX_AUTH_FAILURES: u8 = 5;

/// The delay before a Host answers the first failed authentication; each
/// further failure doubles it (M2-124 section 6.7).
const AUTH_FAILURE_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub(crate) struct Challenged {
    /// What the Client was challenged with.
    challenge: Challenge,
    nonce: Nonce,
    since: Instant,
    failures: u8,
    /// A reply held back so a guessing peer is slowed.
    delayed: Option<(Instant, Command)>,
}

/// The states of a Host before the Session is established.
#[derive(Clone, Debug)]
pub(crate) enum HostOpening {
    /// The Host waits for its application to accept or decline.
    Confirming { since: Instant },
    /// The Host waits for the answer to its challenge.
    Challenged(Box<Challenged>),
}

/// Whether a Host has room for another Client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Capacity {
    Available,
    Full,
}

/// What a Host holds that a Client does not: how it admits, the source of its
/// nonces and whether it has room.
#[derive(Debug)]
pub(crate) struct HostRole {
    admission: Arc<Admission>,
    nonce_source: fn() -> Result<Nonce, getrandom::Error>,
    capacity: Capacity,
}

impl Session<HostRole> {
    pub(crate) fn host(
        admission: Arc<Admission>,
        nonce_source: fn() -> Result<Nonce, getrandom::Error>,
        capacity: Capacity,
        cfg: SessionConfig,
        local: EndpointIdentity,
        peer: SocketAddr,
        now: Instant,
    ) -> Self {
        let role = HostRole {
            admission,
            nonce_source,
            capacity,
        };
        Self::new(role, cfg, local, peer, now)
    }

    /// Whether the peer is in a Session, or an Invitation or challenge is
    /// open with it: what a Host keeps an entry for.
    pub(crate) fn holds_session(&self) -> bool {
        matches!(
            self.state(),
            SessionState::PendingInvitation
                | SessionState::AuthenticationRequired
                | SessionState::Established
        )
    }

    /// Begins the Session after the application accepted the Invitation it was
    /// asked about.
    pub(crate) fn accept(&mut self, now: Instant) {
        if matches!(
            self.core.phase,
            Phase::Opening(HostOpening::Confirming { .. })
        ) {
            self.role.admit(&mut self.core, now);
        }
    }

    pub(crate) fn decline(&mut self, now: Instant) {
        if matches!(
            self.core.phase,
            Phase::Opening(HostOpening::Confirming { .. })
        ) {
            self.core.refuse(now, ByeReason::UserDeclined);
        }
    }
}

impl HostRole {
    /// Tells the Client it is accepted and begins the Session.
    fn admit(&self, core: &mut Core<Self>, now: Instant) {
        core.out.push(Command::Accepted(core.local));
        core.establish(now);
    }

    /// Checks an authenticated Invitation against the challenge in hand.
    /// `judge` answers `None` for a method the challenge is not of.
    fn authenticate(
        &self,
        core: &mut Core<Self>,
        now: Instant,
        header: Header,
        judge: impl FnOnce(&Challenge, &Nonce) -> Option<Verdict>,
    ) {
        let local = core.local;
        match &mut core.phase {
            Phase::Opening(HostOpening::Challenged(c)) => {
                let Some(verdict) = judge(&c.challenge, &c.nonce) else {
                    return core.nak(NakReason::NotExpected, header);
                };
                if verdict == Verdict::Right {
                    return self.admit(core, now);
                }
                c.failures += 1;
                if c.failures >= MAX_AUTH_FAILURES {
                    let reason = if verdict == Verdict::UnknownUser {
                        ByeReason::UsernameNotFound
                    } else {
                        ByeReason::AuthenticationFailed
                    };
                    return core.refuse(now, reason);
                }
                let because = if verdict == Verdict::UnknownUser {
                    Because::UserNotFound
                } else {
                    Because::DigestWrong
                };
                let again = c.challenge.command(c.nonce, because, local);
                let delay = AUTH_FAILURE_DELAY * 2u32.pow(u32::from(c.failures) - 1);
                c.delayed = Some((now + delay, again));
            }
            Phase::Established | Phase::Resetting(_) => core.out.push(Command::Accepted(local)),
            Phase::Closing { .. } | Phase::Opening(HostOpening::Confirming { .. }) => {}
            Phase::Idle => core.refuse(now, ByeReason::NoPriorInvitation),
        }
    }
}

impl Role for HostRole {
    type Opening = HostOpening;

    fn opening_state(opening: &HostOpening) -> SessionState {
        match opening {
            HostOpening::Confirming { .. } => SessionState::PendingInvitation,
            HostOpening::Challenged(_) => SessionState::AuthenticationRequired,
        }
    }

    fn opening_deadline(opening: &HostOpening, cfg: &SessionConfig) -> Instant {
        match opening {
            HostOpening::Confirming { since } => *since + cfg.pending_timeout.get(),
            HostOpening::Challenged(c) => {
                let expires = c.since + cfg.pending_timeout.get();
                c.delayed
                    .as_ref()
                    .map_or(expires, |(at, _)| expires.min(*at))
            }
        }
    }

    fn tick_opening(&mut self, core: &mut Core<Self>, now: Instant) {
        let cfg = core.cfg;
        match &mut core.phase {
            Phase::Opening(HostOpening::Confirming { since }) => {
                if now >= *since + cfg.pending_timeout.get() {
                    core.refuse(now, ByeReason::UserDeclined);
                }
            }
            Phase::Opening(HostOpening::Challenged(c)) => {
                if let Some((at, _)) = &c.delayed
                    && now >= *at
                    && let Some((_, command)) = c.delayed.take()
                {
                    core.out.push(command);
                }
                if now >= c.since + cfg.pending_timeout.get() {
                    core.phase = Phase::Idle;
                    core.remote = None;
                }
            }
            _ => {}
        }
    }

    fn on_invitation(
        &mut self,
        core: &mut Core<Self>,
        now: Instant,
        _header: Header,
        invitation: Invitation,
    ) {
        let local = core.local;
        match &core.phase {
            // A peer the Host already admitted asks again: it missed the reply.
            Phase::Established | Phase::Resetting(_) => {
                return core.out.push(Command::Accepted(local));
            }
            Phase::Opening(HostOpening::Confirming { .. }) => {
                return core.out.push(Command::Pending(local));
            }
            Phase::Opening(HostOpening::Challenged(c)) => {
                let again = c.challenge.command(c.nonce, Because::Invited, local);
                return core.out.push(again);
            }
            Phase::Closing { .. } => return,
            Phase::Idle => {}
        }
        if self.capacity == Capacity::Full {
            return core.refuse(now, ByeReason::TooManySessions);
        }
        core.remote = Some(invitation.identity);
        if let Some(challenge) = self.admission.challenge() {
            if !invitation.methods.contains(challenge.required()) {
                return core.refuse(now, ByeReason::NoMatchingAuthentication);
            }
            let nonce = match (self.nonce_source)() {
                Ok(nonce) => nonce,
                Err(error) => {
                    core.events.push(Event::NonceUnavailable {
                        peer: core.peer,
                        error,
                    });
                    return core.refuse(now, ByeReason::Unknown);
                }
            };
            core.out
                .push(challenge.command(nonce, Because::Invited, local));
            core.phase = Phase::Opening(HostOpening::Challenged(Box::new(Challenged {
                challenge,
                nonce,
                since: now,
                failures: 0,
                delayed: None,
            })));
        } else if *self.admission == Admission::Confirm {
            core.out.push(Command::Pending(local));
            core.phase = Phase::Opening(HostOpening::Confirming { since: now });
            core.events.push(Event::Confirm {
                peer: core.peer,
                remote: invitation.identity,
            });
        } else {
            self.admit(core, now);
        }
    }

    fn on_auth(&mut self, core: &mut Core<Self>, now: Instant, header: Header, digest: &Digest) {
        self.authenticate(core, now, header, |challenge, nonce| {
            challenge.judge_secret(nonce, digest)
        });
    }

    fn on_user_auth(
        &mut self,
        core: &mut Core<Self>,
        now: Instant,
        header: Header,
        auth: &UserAuthentication,
    ) {
        self.authenticate(core, now, header, |challenge, nonce| {
            challenge.judge_user(nonce, auth)
        });
    }
}
