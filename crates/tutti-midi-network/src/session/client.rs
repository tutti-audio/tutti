//! The Client role: it invites a Host and answers what the Host replies.

use std::net::SocketAddr;
use tutti_platform::Instant;

use super::{Core, Phase, Repeat, Role, Session};
use crate::{
    AuthChallenge, AuthFailure, AuthState, ByeReason, Command, Credentials, Digest,
    EndpointIdentity, Header, Invitation, NakReason, SessionConfig, SessionState,
    UserAuthChallenge, UserAuthState, UserAuthentication,
};

/// The states of a Client before the Session is established.
#[derive(Clone, Debug)]
pub(crate) enum ClientOpening {
    /// The Invitation, or the answer to a challenge, is repeated.
    Inviting(Repeat),
    /// A Host said Pending: the Client waits, repeating nothing.
    AwaitingAcceptance { until: Instant },
}

/// What a Client holds that a Host does not: the secrets it answers with.
#[derive(Debug)]
pub(crate) struct ClientRole {
    credentials: Credentials,
}

impl Session<ClientRole> {
    pub(crate) fn client(
        credentials: Credentials,
        cfg: SessionConfig,
        local: EndpointIdentity,
        peer: SocketAddr,
        now: Instant,
    ) -> Self {
        Self::new(ClientRole { credentials }, cfg, local, peer, now)
    }

    /// The Client's first move: the Invitation.
    pub(crate) fn invite(&mut self, now: Instant) {
        let (core, role) = (&mut self.core, &self.role);
        if !matches!(core.phase, Phase::Idle) {
            return;
        }
        let invitation = Command::Invitation(Invitation {
            methods: role.credentials.methods(),
            identity: core.local,
        });
        core.last_heard = now;
        let repeat = core.start_repeat(now, invitation);
        core.phase = Phase::Opening(ClientOpening::Inviting(repeat));
    }
}

impl ClientRole {
    /// The credentials, while the Invitation is out and unanswered. A challenge
    /// is expected only then (M2-124 sections 6.7 and 6.8), and only for a
    /// method the Invitation advertised, which is the method these credentials
    /// hold.
    fn inviting_credentials(&self, core: &Core<Self>) -> Option<&Credentials> {
        matches!(core.phase, Phase::Opening(_)).then_some(&self.credentials)
    }
}

impl Role for ClientRole {
    type Opening = ClientOpening;

    fn opening_state(_: &ClientOpening) -> SessionState {
        SessionState::PendingInvitation
    }

    fn opening_deadline(opening: &ClientOpening, cfg: &SessionConfig) -> Instant {
        match opening {
            ClientOpening::Inviting(r) => r.deadline(cfg),
            ClientOpening::AwaitingAcceptance { until } => *until,
        }
    }

    fn tick_opening(&mut self, core: &mut Core<Self>, now: Instant) {
        let cfg = core.cfg;
        match &mut core.phase {
            Phase::Opening(ClientOpening::Inviting(r)) => {
                if now >= r.started + cfg.command_timeout.get() {
                    return core.close(now, ByeReason::InvitationCanceled);
                }
                r.repeat_due(now, &cfg, &mut core.out);
            }
            Phase::Opening(ClientOpening::AwaitingAcceptance { until }) if now >= *until => {
                core.close(now, ByeReason::InvitationCanceled);
            }
            _ => {}
        }
    }

    fn on_accepted(
        &mut self,
        core: &mut Core<Self>,
        now: Instant,
        _header: Header,
        identity: EndpointIdentity,
    ) {
        match core.phase {
            Phase::Opening(_) => {
                core.remote = Some(identity);
                core.establish(now);
            }
            Phase::Established | Phase::Resetting(_) => {}
            Phase::Idle | Phase::Closing { .. } => {
                core.refuse(now, ByeReason::NoPendingInvitation);
            }
        }
    }

    fn on_pending(
        &mut self,
        core: &mut Core<Self>,
        now: Instant,
        _header: Header,
        identity: EndpointIdentity,
    ) {
        match core.phase {
            Phase::Opening(_) => {
                core.remote = Some(identity);
                core.phase = Phase::Opening(ClientOpening::AwaitingAcceptance {
                    until: now + core.cfg.pending_timeout.get(),
                });
            }
            Phase::Established | Phase::Resetting(_) => {}
            Phase::Idle | Phase::Closing { .. } => {
                core.refuse(now, ByeReason::NoPendingInvitation);
            }
        }
    }

    fn on_challenge(
        &mut self,
        core: &mut Core<Self>,
        now: Instant,
        header: Header,
        challenge: &AuthChallenge,
    ) {
        let digest = self
            .inviting_credentials(core)
            .and_then(|credentials| credentials.secret.as_deref())
            .map(|secret| Digest::of_secret(&challenge.nonce, secret));
        let Some(digest) = digest else {
            return core.nak(NakReason::NotExpected, header);
        };
        if challenge.state == AuthState::DigestWrong {
            return core.authentication_failed(now, AuthFailure::DigestWrong);
        }
        core.remote = Some(challenge.identity);
        let repeat = core.start_repeat(now, Command::InvitationWithAuth(digest));
        core.phase = Phase::Opening(ClientOpening::Inviting(repeat));
    }

    fn on_user_challenge(
        &mut self,
        core: &mut Core<Self>,
        now: Instant,
        header: Header,
        challenge: &UserAuthChallenge,
    ) {
        let answer = self
            .inviting_credentials(core)
            .and_then(|credentials| credentials.user.as_ref())
            .map(|(username, password)| {
                Box::new(UserAuthentication {
                    digest: Digest::of_user(&challenge.nonce, username, password),
                    username: *username,
                })
            });
        let Some(answer) = answer else {
            return core.nak(NakReason::NotExpected, header);
        };
        match challenge.state {
            UserAuthState::DigestWrong => {
                return core.authentication_failed(now, AuthFailure::DigestWrong);
            }
            UserAuthState::UserNotFound => {
                return core.authentication_failed(now, AuthFailure::UserNotFound);
            }
            UserAuthState::First => {}
        }
        core.remote = Some(challenge.identity);
        let repeat = core.start_repeat(now, Command::InvitationWithUserAuth(answer));
        core.phase = Phase::Opening(ClientOpening::Inviting(repeat));
    }
}
