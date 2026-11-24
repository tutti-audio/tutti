//! What a Host's [`Admission`](crate::Admission) asks of a Client, and how it
//! judges the answer.

use crate::{
    AuthChallenge, AuthMethods, AuthState, Command, Digest, EndpointIdentity, Nonce,
    UserAuthChallenge, UserAuthState, UserAuthentication, Username,
};

/// The secrets an [`Admission`](crate::Admission) that challenges holds.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Challenge<'a> {
    SharedSecret(&'a str),
    Users(&'a [(Username, String)]),
}

/// Why a challenge is issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Because {
    /// The first challenge, in answer to an Invitation.
    Invited,
    DigestWrong,
    UserNotFound,
}

/// What a Host concludes from an answer to its challenge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The digest is right.
    Right,
    /// The digest is wrong.
    Wrong,
    /// The user name is not known.
    UnknownUser,
}

impl Challenge<'_> {
    /// The method a Client must advertise to be challenged.
    pub(crate) fn required(self) -> AuthMethods {
        match self {
            Self::SharedSecret(_) => AuthMethods::SHARED_SECRET,
            Self::Users(_) => AuthMethods::USER,
        }
    }

    /// The challenge command, with the state `because` maps to.
    pub(crate) fn command(
        self,
        nonce: Nonce,
        because: Because,
        identity: EndpointIdentity,
    ) -> Command {
        match self {
            Self::SharedSecret(_) => Command::AuthRequired(Box::new(AuthChallenge {
                state: match because {
                    Because::Invited => AuthState::First,
                    Because::DigestWrong | Because::UserNotFound => AuthState::DigestWrong,
                },
                nonce,
                identity,
            })),
            Self::Users(_) => Command::UserAuthRequired(Box::new(UserAuthChallenge {
                state: match because {
                    Because::Invited => UserAuthState::First,
                    Because::DigestWrong => UserAuthState::DigestWrong,
                    Because::UserNotFound => UserAuthState::UserNotFound,
                },
                nonce,
                identity,
            })),
        }
    }

    /// The verdict on an Invitation with Authentication, or `None` when the
    /// challenge is not a shared secret's.
    pub(crate) fn judge_secret(self, nonce: &Nonce, answer: &Digest) -> Option<Verdict> {
        match self {
            Self::SharedSecret(secret) => Some(verdict(&Digest::of_secret(nonce, secret), answer)),
            Self::Users(_) => None,
        }
    }

    /// The verdict on an Invitation with User Authentication, or `None` when
    /// the challenge is not a user's.
    pub(crate) fn judge_user(self, nonce: &Nonce, answer: &UserAuthentication) -> Option<Verdict> {
        match self {
            Self::Users(users) => Some(
                users
                    .iter()
                    .find(|(name, _)| name == &answer.username)
                    .map_or(Verdict::UnknownUser, |(name, password)| {
                        verdict(&Digest::of_user(nonce, name, password), &answer.digest)
                    }),
            ),
            Self::SharedSecret(_) => None,
        }
    }
}

/// Right when the digest is the one the Host computes.
fn verdict(expected: &Digest, got: &Digest) -> Verdict {
    if expected.matches(got) {
        Verdict::Right
    } else {
        Verdict::Wrong
    }
}
