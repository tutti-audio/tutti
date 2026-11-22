//! A Client and a Host over a simulated network: Session establishment and
//! authentication, data transfer with loss, keep-alive, reset and Bye.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_units::Losses;

use tutti_midi_network::{
    Admission, AuthChallenge, AuthFailure, AuthMethods, AuthState, Bye, ByeReason, Client, Command,
    Credentials, DatagramBuilder, Digest, EndedBy, Event, Fec, Host, HostConfig, IdleInterval,
    Invitation, MAX_QUEUED_EVENTS, NakReason, NetworkEndpoint, Nonce, PingId, Requests, Retransmit,
    RetransmitErrorReason, RetransmitRefusal, RetransmitRequest, SendError, SequenceNumber,
    SessionConfig, SessionState, Timer, UmpData, UserAuthChallenge, UserAuthState, Username,
};

use crate::sim::{Direction, Fate, Sim, client_addr, fixed_nonce, host_addr, identity, note_on};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn datagram(command: &Command) -> Vec<u8> {
    let mut b = DatagramBuilder::new();
    b.push(command);
    b.finish()
}

fn host_with(admission: Admission) -> HostConfig {
    let mut config = HostConfig::new(identity("Studio", "HOST-1"));
    config.admission = admission;
    config
}

fn established() -> Sim {
    let mut sim = Sim::open();
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    sim.client_events.clear();
    sim.host_events.clear();
    sim.log.clear();
    sim
}

fn data_commands(sim: &Sim, dir: Direction) -> Vec<UmpData> {
    sim.commands(dir)
        .into_iter()
        .filter_map(|c| match c {
            Command::UmpData(d) => Some(d),
            _ => None,
        })
        .collect()
}

// ---- Establishing a Session ----

/// Spec: m2-124/6.4#1, m2-124/6.5#1, m2-124/6.5#5, m2-124/6.5#8, m2-124/6.4#6
#[test]
fn an_invitation_to_an_open_host_establishes_both_ends_and_each_learns_the_other() {
    let mut sim = Sim::open();
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
    assert_eq!(
        sim.client_events,
        vec![Event::Established {
            peer: host_addr(),
            remote: identity("Studio", "HOST-1")
        }]
    );
    assert_eq!(
        sim.host_events,
        vec![Event::Established {
            peer: client_addr(),
            remote: identity("Laptop", "CLIENT-1")
        }]
    );
}

/// Spec: m2-124/6.6#1, m2-124/6.6#2, m2-124/6.4#6
#[test]
fn a_pending_reply_waits_for_the_application_then_accepts() {
    let mut sim = Sim::new(
        host_with(Admission::Confirm),
        Credentials::none(),
        SessionConfig::default(),
    );
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::PendingInvitation);
    assert!(matches!(sim.host_events[..], [Event::Confirm { .. }]));
    // The client stops repeating while the host is pending.
    let invitations_before = sim.commands(Direction::ClientToHost).len();
    sim.advance(ms(2000));
    let invitations: usize = sim
        .commands(Direction::ClientToHost)
        .iter()
        .filter(|c| matches!(c, Command::Invitation(_)))
        .count();
    assert_eq!(invitations, 1, "no repeat after a Pending reply");
    assert!(sim.commands(Direction::ClientToHost).len() >= invitations_before);
    sim.host.accept(sim.now, client_addr());
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

/// Spec: m2-124/6.4#6
#[test]
fn a_declined_invitation_ends_with_the_user_declined_reason() {
    let mut sim = Sim::new(
        host_with(Admission::Confirm),
        Credentials::none(),
        SessionConfig::default(),
    );
    sim.connect();
    sim.host.decline(sim.now, client_addr());
    sim.flush();
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        ended_by: EndedBy::Remote,
        reason: ByeReason::UserDeclined
    }));
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

/// Spec: m2-124/6.4#3
#[test]
fn a_lost_invitation_is_repeated_and_the_lost_reply_is_answered_again() {
    let mut sim = Sim::open();
    let mut dropped = 0;
    sim.set_fate(move |dir, _| {
        if dir == Direction::HostToClient && dropped < 1 {
            dropped += 1;
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::PendingInvitation);
    sim.advance(ms(500));
    assert_eq!(sim.client.state(), SessionState::Established);
    let invitations = sim
        .commands(Direction::ClientToHost)
        .iter()
        .filter(|c| matches!(c, Command::Invitation(_)))
        .count();
    assert_eq!(invitations, 2);
}

/// Spec: m2-124/6.4#4, m2-124/6.4#5, m2-124/6.2#3
#[test]
fn an_unanswered_invitation_is_cancelled_with_a_bye_after_the_timeout() {
    let mut sim = Sim::open();
    sim.set_fate(|_, _| Fate::Drop);
    sim.connect();
    sim.advance(Duration::from_secs(5));
    let sent = sim.commands(Direction::ClientToHost);
    assert!(
        sent.iter()
            .filter(|c| matches!(c, Command::Invitation(_)))
            .count()
            >= 5
    );
    assert!(sent.contains(&Command::Bye(Box::new(Bye::new(
        ByeReason::InvitationCanceled
    )))));
    sim.advance(Duration::from_secs(6));
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        ended_by: EndedBy::Local,
        reason: ByeReason::InvitationCanceled
    }));
}

/// Spec: m2-124/6.4#8
#[test]
fn a_repeated_invitation_in_an_established_session_is_accepted_again() {
    let mut sim = established();
    let again = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("Laptop", "CLIENT-1"),
    }));
    sim.host.on_message(client_addr(), &again, sim.now);
    sim.flush();
    assert!(matches!(
        sim.commands(Direction::HostToClient)[..],
        [Command::Accepted(_)]
    ));
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

#[test]
fn a_malformed_invitation_is_a_protocol_error_bye() {
    let mut sim = Sim::open();
    // Name only: no Product Instance Id.
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0x01, 0x01, 0x01, 0x00, b'a', 0, 0, 0]);
    sim.host.on_message(client_addr(), &bytes, sim.now);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Box::new(Bye::new(ByeReason::ProtocolError)))]
    );
}

/// Spec: m2-124/8#4
#[test]
fn a_host_at_its_limit_turns_a_newcomer_away_with_too_many_sessions() {
    let mut config = host_with(Admission::Open);
    config.max_sessions = std::num::NonZeroUsize::MIN;
    let mut sim = Sim::new(config, Credentials::none(), SessionConfig::default());
    sim.connect();
    let other = "10.0.0.3:6000".parse().unwrap();
    let inv = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("Other", "OTHER-1"),
    }));
    sim.host.on_message(other, &inv, sim.now);
    let reply = sim.host.poll_transmit().unwrap();
    assert_eq!(reply.to, other);
    let commands: Vec<_> = tutti_midi_network::parse(&reply.payload)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        commands,
        vec![Command::Bye(Box::new(Bye::new(ByeReason::TooManySessions)))]
    );
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

/// Spec: m2-124/6.5#4
#[test]
fn an_accepted_reply_with_no_invitation_gets_a_bye_for_no_pending_invitation() {
    let mut sim = Sim::open();
    let accepted = datagram(&Command::Accepted(identity("Studio", "HOST-1")));
    sim.client.on_message(host_addr(), &accepted, sim.now);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::ClientToHost),
        vec![Command::Bye(Box::new(Bye::new(
            ByeReason::NoPendingInvitation
        )))]
    );
}

/// Spec: m2-124/6.4#9
#[test]
fn a_client_without_a_host_naks_an_invitation_as_not_expected() {
    let mut sim = Sim::open();
    let inv = Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("X", "Y"),
    });
    sim.client.on_message(host_addr(), &datagram(&inv), sim.now);
    sim.flush();
    let [Command::Nak(nak)] = &sim.commands(Direction::ClientToHost)[..] else {
        panic!("expected a NAK")
    };
    assert_eq!(nak.reason, NakReason::NotExpected);
    assert_eq!(nak.refused, inv.header());
}

// ---- Authentication ----

fn secret_sim(client_secret: &str) -> Sim {
    Sim::new(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none().secret(client_secret),
        SessionConfig::default(),
    )
}

/// Spec: m2-124/6.7#1, m2-124/6.7#3, m2-124/6.7#4, m2-124/6.7#12, m2-124/6.9#1, m2-124/6.9#3, m2-124/6.4#6
#[test]
fn the_right_shared_secret_establishes_the_session() {
    let mut sim = secret_sim("5483");
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    let to_client = sim.commands(Direction::HostToClient);
    assert!(matches!(
        to_client[0],
        Command::AuthRequired(ref c) if c.state == AuthState::First && c.nonce == fixed_nonce()
    ));
    let to_host = sim.commands(Direction::ClientToHost);
    assert!(
        to_host.contains(&Command::InvitationWithAuth(Digest::of_secret(
            &fixed_nonce(),
            "5483"
        )))
    );
}

/// Spec: m2-124/6.7#7, m2-124/6.7#8, m2-124/6.7#13, m2-124/6.9#4
#[test]
fn a_wrong_secret_is_answered_after_a_delay_and_the_client_gives_up() {
    let mut sim = secret_sim("0000");
    sim.connect();
    let challenges = |sim: &Sim| {
        sim.commands(Direction::HostToClient)
            .into_iter()
            .filter(|c| matches!(c, Command::AuthRequired(_)))
            .count()
    };
    assert_eq!(challenges(&sim), 1);
    sim.advance(ms(50));
    assert_eq!(
        challenges(&sim),
        1,
        "the reply to a wrong digest is held back"
    );
    sim.advance(ms(60));
    assert_eq!(challenges(&sim), 2);
    assert!(
        sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(
                c,
                Command::AuthRequired(c) if c.state == AuthState::DigestWrong
            ))
    );
    assert!(sim.client_events.contains(&Event::AuthenticationFailed {
        peer: host_addr(),
        cause: AuthFailure::DigestWrong
    }));
    assert!(
        sim.commands(Direction::ClientToHost)
            .contains(&Command::Bye(Box::new(Bye::new(
                ByeReason::InvitationCanceled
            ))))
    );
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

/// Spec: m2-124/6.7#8
#[test]
fn the_delay_after_each_failed_digest_doubles() {
    let mut sim = secret_sim("5483");
    let guesser = "10.0.0.9:7000".parse().unwrap();
    let inv = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::SHARED_SECRET,
        identity: identity("Guess", "G-1"),
    }));
    let wrong = datagram(&Command::InvitationWithAuth(Digest::new([0; 32])));
    let drain = |sim: &mut Sim| {
        let mut n = 0;
        while sim.host.poll_transmit().is_some() {
            n += 1;
        }
        n
    };
    let start = sim.now;
    sim.host.on_message(guesser, &inv, start);
    assert_eq!(drain(&mut sim), 1, "the challenge");
    // First failure: 100 ms.
    sim.host.on_message(guesser, &wrong, start);
    sim.host.on_timeout(start + ms(99));
    assert_eq!(drain(&mut sim), 0);
    sim.host.on_timeout(start + ms(100));
    assert_eq!(drain(&mut sim), 1);
    // Second failure: 200 ms.
    let second = start + ms(100);
    sim.host.on_message(guesser, &wrong, second);
    sim.host.on_timeout(second + ms(199));
    assert_eq!(drain(&mut sim), 0);
    sim.host.on_timeout(second + ms(200));
    assert_eq!(drain(&mut sim), 1);
}

/// Spec: m2-124/6.7#13
#[test]
fn five_failed_digests_end_the_invitation_with_authentication_failed() {
    let mut sim = Sim::new(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none().secret("5483"),
        SessionConfig::default(),
    );
    // A guesser invites, and answers every challenge with a wrong digest.
    let guesser = "10.0.0.9:7000".parse().unwrap();
    let inv = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::SHARED_SECRET,
        identity: identity("Guess", "G-1"),
    }));
    let wrong = datagram(&Command::InvitationWithAuth(Digest::new([0; 32])));
    sim.host.on_message(guesser, &inv, sim.now);
    for _ in 0..5 {
        sim.host.on_message(guesser, &wrong, sim.now);
    }
    let mut saw_bye = false;
    while let Some(t) = sim.host.poll_transmit() {
        for c in tutti_midi_network::parse(&t.payload)
            .unwrap()
            .map(Result::unwrap)
        {
            saw_bye |= c == Command::Bye(Box::new(Bye::new(ByeReason::AuthenticationFailed)));
        }
    }
    assert!(saw_bye);
}

/// A Host that needs a method the Invitation did not flag sends no challenge
/// of any kind: its one reply is the Bye.
/// Spec: m2-124/6.4#13, m2-124/6.7#2, m2-124/6.8#2
#[test]
fn a_client_with_no_matching_credentials_is_told_there_is_no_matching_method() {
    let users = || Admission::Users(vec![(Username::new("Rosa").unwrap(), "RPBqBno".into())]);
    let cases = [
        (Admission::SharedSecret("5483".into()), Credentials::none()),
        (
            Admission::SharedSecret("5483".into()),
            Credentials::none().user(Username::new("Rosa").unwrap(), "RPBqBno"),
        ),
        (users(), Credentials::none()),
        (users(), Credentials::none().secret("5483")),
    ];
    for (admission, credentials) in cases {
        let mut sim = Sim::new(host_with(admission), credentials, SessionConfig::default());
        sim.connect();
        assert!(sim.client_events.contains(&Event::Closed {
            peer: host_addr(),
            ended_by: EndedBy::Remote,
            reason: ByeReason::NoMatchingAuthentication
        }));
        assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
        assert_eq!(
            sim.commands(Direction::HostToClient),
            vec![Command::Bye(Box::new(Bye::new(
                ByeReason::NoMatchingAuthentication
            )))],
            "no challenge before the Bye"
        );
    }
}

static NONCE_CALLS: AtomicU8 = AtomicU8::new(0);

fn counting_nonce() -> Result<Nonce, getrandom::Error> {
    let n = NONCE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(Nonce::new([b'a' + n; 16]).unwrap())
}

fn no_nonce() -> Result<Nonce, getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}

// A Host that cannot get random bytes for a challenge refuses the Invitation
// that needed it, and tells its application why; it does not panic.
#[test]
fn a_host_without_a_nonce_refuses_the_invitation_that_needs_one() {
    let mut sim = Sim::with_nonces(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none().secret("5483"),
        SessionConfig::default(),
        no_nonce,
    );
    sim.connect();
    assert!(sim.host_events.contains(&Event::NonceUnavailable {
        peer: client_addr(),
        error: getrandom::Error::UNSUPPORTED,
    }));
    assert_ne!(sim.client.state(), SessionState::Established);
}

/// Spec: m2-124/6.7#5, m2-124/6.7#6
#[test]
fn a_repeated_invitation_gets_the_same_nonce_and_a_new_session_a_new_one() {
    NONCE_CALLS.store(0, Ordering::SeqCst);
    let mut sim = Sim::with_nonces(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none().secret("5483"),
        SessionConfig::default(),
        counting_nonce,
    );
    // The first challenge is lost, so the client repeats its invitation.
    let mut dropped = false;
    sim.set_fate(move |dir, _| {
        if dir == Direction::HostToClient && !dropped {
            dropped = true;
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.connect();
    sim.advance(ms(500));
    assert_eq!(sim.client.state(), SessionState::Established);
    let nonces: Vec<Nonce> = sim
        .commands(Direction::HostToClient)
        .into_iter()
        .filter_map(|c| match c {
            Command::AuthRequired(c) => Some(c.nonce),
            _ => None,
        })
        .collect();
    assert_eq!(nonces.len(), 2);
    assert_eq!(nonces[0], nonces[1]);
    assert_eq!(NONCE_CALLS.load(Ordering::SeqCst), 1);
    // A fresh invitation from the same peer after the session ends is new.
    sim.client.close(sim.now, ByeReason::UserTerminated);
    sim.flush();
    sim.log.clear();
    let mut again = tutti_midi_network::Client::new(
        identity("Laptop", "CLIENT-1"),
        Credentials::none().secret("5483"),
        SessionConfig::default(),
        host_addr(),
        sim.now,
    );
    again.connect(sim.now);
    let t = again.poll_transmit().unwrap();
    sim.host.on_message(client_addr(), &t.payload, sim.now);
    sim.flush();
    let fresh = sim.commands(Direction::HostToClient);
    assert!(matches!(
        fresh[0],
        Command::AuthRequired(ref c) if c.nonce != nonces[0]
    ));
}

/// Spec: m2-124/6.9#5
#[test]
fn an_authenticated_invitation_without_an_invitation_is_rejected_for_no_prior_invitation() {
    let mut sim = secret_sim("5483");
    let auth = datagram(&Command::InvitationWithAuth(Digest::new([0; 32])));
    sim.host.on_message(client_addr(), &auth, sim.now);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Box::new(Bye::new(
            ByeReason::NoPriorInvitation
        )))]
    );
}

fn users_sim(user: &str, password: &str) -> Sim {
    Sim::new(
        host_with(Admission::Users(vec![(
            Username::new("Rosa").unwrap(),
            "RPBqBno".into(),
        )])),
        Credentials::none().user(Username::new(user).unwrap(), password),
        SessionConfig::default(),
    )
}

/// Spec: m2-124/6.8#1, m2-124/6.8#3, m2-124/6.8#4, m2-124/6.8#10, m2-124/6.10#1, m2-124/6.10#4
#[test]
fn a_known_user_with_the_right_password_establishes_the_session() {
    let mut sim = users_sim("Rosa", "RPBqBno");
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert!(matches!(
        sim.commands(Direction::HostToClient)[0],
        Command::UserAuthRequired(ref c) if c.state == UserAuthState::First
    ));
}

/// Spec: m2-124/6.8#11, m2-124/6.10#5, m2-124/6.10#4
#[test]
fn an_unknown_user_is_told_the_user_was_not_found() {
    let mut sim = users_sim("Bob", "RPBqBno");
    sim.connect();
    sim.advance(ms(150));
    assert!(
        sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(
                c,
                Command::UserAuthRequired(c) if c.state == UserAuthState::UserNotFound
            ))
    );
    assert!(sim.client_events.contains(&Event::AuthenticationFailed {
        peer: host_addr(),
        cause: AuthFailure::UserNotFound
    }));
}

#[test]
fn a_wrong_password_is_told_the_digest_is_wrong() {
    let mut sim = users_sim("Rosa", "nope");
    sim.connect();
    sim.advance(ms(150));
    assert!(sim.client_events.contains(&Event::AuthenticationFailed {
        peer: host_addr(),
        cause: AuthFailure::DigestWrong
    }));
}

/// What a Client answers when a challenge reaches it: the NAKs, as a Host
/// would see them.
fn naks_from_client(sim: &Sim) -> Vec<(NakReason, u32)> {
    sim.commands(Direction::ClientToHost)
        .into_iter()
        .filter_map(|c| match c {
            Command::Nak(n) => Some((n.reason, n.refused.word())),
            _ => None,
        })
        .collect()
}

fn auth_required() -> Command {
    Command::AuthRequired(Box::new(AuthChallenge {
        state: AuthState::First,
        nonce: fixed_nonce(),
        identity: identity("Studio", "HOST-1"),
    }))
}

fn user_auth_required() -> Command {
    Command::UserAuthRequired(Box::new(UserAuthChallenge {
        state: UserAuthState::First,
        nonce: fixed_nonce(),
        identity: identity("Studio", "HOST-1"),
    }))
}

/// Spec: m2-124/6.7#10, m2-124/6.8#8
#[test]
fn a_client_that_sent_no_invitation_naks_a_challenge_as_not_expected() {
    for challenge in [auth_required(), user_auth_required()] {
        let mut sim = Sim::new(
            HostConfig::new(identity("Studio", "HOST-1")),
            Credentials::none().secret("1234"),
            SessionConfig::default(),
        );
        sim.client
            .on_message(host_addr(), &datagram(&challenge), sim.now);
        sim.flush();
        assert_eq!(
            naks_from_client(&sim),
            vec![(NakReason::NotExpected, challenge.header().word())]
        );
        assert_eq!(sim.client.state(), SessionState::Idle);
    }
}

/// M2-124 sections 6.7 and 6.8: a Client that did not advertise the method in
/// its Invitation answers its challenge with NAK 0x02 (Command Not Expected).
/// Spec: m2-124/6.7#11, m2-124/6.8#9
#[test]
fn a_client_naks_a_challenge_for_a_method_its_invitation_did_not_advertise() {
    let cases = [
        // Holds a user, advertised USER: a shared-secret challenge is unexpected.
        (
            Credentials::none().user(Username::new("Rosa").unwrap(), "pw"),
            auth_required(),
        ),
        // Holds a secret, advertised SHARED_SECRET: a user challenge is unexpected.
        (Credentials::none().secret("1234"), user_auth_required()),
        // Advertised nothing: neither is expected.
        (Credentials::none(), auth_required()),
        (Credentials::none(), user_auth_required()),
    ];
    for (credentials, challenge) in cases {
        // A Host that holds the Invitation pending leaves it unanswered.
        let mut sim = Sim::new(
            host_with(Admission::Confirm),
            credentials,
            SessionConfig::default(),
        );
        sim.client.connect(sim.now);
        sim.client
            .on_message(host_addr(), &datagram(&challenge), sim.now);
        sim.flush();
        assert_eq!(
            naks_from_client(&sim),
            vec![(NakReason::NotExpected, challenge.header().word())]
        );
        assert_eq!(sim.client.state(), SessionState::PendingInvitation);
        assert!(
            !sim.client_events
                .iter()
                .any(|e| matches!(e, Event::AuthenticationFailed { .. })),
            "an unexpected challenge is not a failed authentication"
        );
    }
}

/// A NAK quotes the refused command's header as it arrived, reserved bits included.
/// Spec: m2-124/5.3#6, m2-124/6.15#4
#[test]
fn a_nak_quotes_the_header_as_received() {
    let mut sim = established();
    // A Ping Reply for a Ping never sent, with its reserved data bits set.
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0x21, 0x01, 0x12, 0x34, 0, 0, 0, 9]);
    sim.client.on_message(host_addr(), &bytes, sim.now);
    sim.flush();
    assert_eq!(
        naks_from_client(&sim),
        vec![(NakReason::BadPingReply, 0x2101_1234)]
    );
}

// ---- Data transfer ----

/// Spec: m2-124/7.1#1, m2-124/6.5#2
#[test]
fn ump_arrives_once_in_order_and_sequence_numbers_start_at_zero() {
    let mut sim = established();
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    assert_eq!(Sim::umps(&sim.host_events), notes);
    let first = data_commands(&sim, Direction::ClientToHost)[0].sequence();
    assert_eq!(first, SequenceNumber::ZERO);
}

/// Spec: m2-124/7.1#4, m2-124/7.2.2#1, m2-124/7.2.2#7
#[test]
fn each_datagram_carries_the_two_previous_commands_before_the_new_one() {
    let mut sim = established();
    for n in 60..63 {
        sim.client.send(sim.now, &[note_on(n)]).unwrap();
    }
    let mut last = None;
    while let Some(t) = sim.client.poll_transmit() {
        last = Some(t);
    }
    let commands: Vec<_> = tutti_midi_network::parse(&last.unwrap().payload)
        .unwrap()
        .map(|c| match c.unwrap() {
            Command::UmpData(d) => d.sequence().get(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        commands,
        vec![0, 1, 2],
        "older commands first, in the order sent"
    );
}

#[test]
fn forward_error_correction_recovers_two_lost_datagrams_with_no_retransmit_request() {
    let mut sim = established();
    let mut seen = 0;
    sim.set_fate(move |dir, _| {
        if dir != Direction::ClientToHost {
            return Fate::Deliver;
        }
        seen += 1;
        // The second and third datagrams are lost.
        if seen == 2 || seen == 3 {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(ms(500));
    assert_eq!(Sim::umps(&sim.host_events), notes);
    assert!(
        !sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(c, Command::RetransmitRequest(_))),
        "FEC filled the gap before any request was due"
    );
}

fn no_fec() -> SessionConfig {
    SessionConfig::default().fec(Fec::Off)
}

fn lossy_sim(client: SessionConfig, lost: Vec<usize>) -> Sim {
    lossy_sim_with(HostConfig::new(identity("Studio", "HOST-1")), client, lost)
}

fn lossy_sim_with(host: HostConfig, client: SessionConfig, lost: Vec<usize>) -> Sim {
    let mut sim = Sim::new(host, Credentials::none(), client);
    sim.connect();
    sim.client_events.clear();
    sim.host_events.clear();
    sim.log.clear();
    let mut seen = 0;
    sim.set_fate(move |dir, bytes| {
        let is_data = bytes.len() > 8 && bytes[4] == 0xFF;
        if dir != Direction::ClientToHost || !is_data {
            return Fate::Deliver;
        }
        seen += 1;
        if lost.contains(&seen) {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim
}

/// Spec: m2-124/7.2.3#1, m2-124/7.2.3#4
#[test]
fn a_missing_command_is_requested_after_the_wait_and_retransmitted_in_order() {
    let mut sim = lossy_sim(no_fec(), vec![2]);
    let notes: Vec<_> = (60..63).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    // Held, not yet delivered past the gap.
    assert_eq!(Sim::umps(&sim.host_events), vec![notes[0]]);
    sim.advance(ms(10));
    assert_eq!(Sim::umps(&sim.host_events), notes);
    assert!(
        sim.commands(Direction::HostToClient)
            .contains(&Command::RetransmitRequest(RetransmitRequest {
                first: SequenceNumber::new(1),
                count: 1
            }))
    );
    assert!(
        !sim.host_events
            .iter()
            .any(|e| matches!(e, Event::DataLost { .. }))
    );
}

/// Spec: m2-124/7.2.4#1, m2-124/7.2.4#2
#[test]
fn a_retransmit_request_the_sender_cannot_serve_reports_the_data_lost() {
    // Only the last command is kept; commands 1 and 2 are gone.
    let mut sim = lossy_sim(
        no_fec()
            .retransmit(Retransmit::Buffer(std::num::NonZeroUsize::MIN))
            .idle_interval(IdleInterval::of(ms(300))),
        vec![2, 3],
    );
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(ms(10));
    assert!(
        sim.commands(Direction::ClientToHost)
            .iter()
            .any(|c| matches!(
                c,
                Command::RetransmitError(e) if e.first == SequenceNumber::new(3)
            ))
    );
    // Command 3 is still kept and was asked for, but the Error answers alone.
    let sent_3 = data_commands(&sim, Direction::ClientToHost)
        .iter()
        .filter(|d| d.sequence() == SequenceNumber::new(3))
        .count();
    assert_eq!(sent_3, 1, "sent once, not resent behind the Error");
    let kinds: Vec<_> = sim
        .host_events
        .iter()
        .filter(|e| matches!(e, Event::Ump { .. } | Event::DataLost { .. }))
        .cloned()
        .collect();
    assert_eq!(
        kinds,
        vec![
            Event::Ump {
                peer: client_addr(),
                packet: notes[0]
            },
            Event::DataLost {
                peer: client_addr(),
                first: SequenceNumber::new(1),
                count: 2
            },
            Event::Ump {
                peer: client_addr(),
                packet: notes[3]
            },
        ]
    );
}

/// Spec: m2-124/7.2.3#10, m2-124/7.2.3#11
#[test]
fn a_peer_without_retransmit_naks_the_request_and_the_gap_is_given_up() {
    let mut sim = lossy_sim(no_fec().retransmit(Retransmit::Unsupported), vec![2]);
    let notes: Vec<_> = (60..63).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(ms(30));
    assert!(
        sim.commands(Direction::ClientToHost)
            .iter()
            .any(|c| matches!(
                c,
                Command::Nak(n) if n.reason == NakReason::NotSupported && n.refused.code() == 0x80
            ))
    );
    assert!(sim.host_events.contains(&Event::DataLost {
        peer: client_addr(),
        first: SequenceNumber::new(1),
        count: 1
    }));
    assert_eq!(Sim::umps(&sim.host_events), vec![notes[0], notes[2]]);
    // No second request after the refusal.
    let requests = sim
        .commands(Direction::HostToClient)
        .iter()
        .filter(|c| matches!(c, Command::RetransmitRequest(_)))
        .count();
    assert_eq!(requests, 1);
}

/// The wait between requests doubles from `reorder_wait`, and an hour is the
/// most any wait is, so the doubling never overflows an `Instant`.
#[test]
fn the_wait_between_retransmit_requests_stops_doubling_at_the_timer_ceiling() {
    let hour = Duration::from_secs(60 * 60);
    let mut host = HostConfig::new(identity("Studio", "HOST-1"));
    host.session = no_fec()
        .reorder_wait(Timer::of(hour))
        .requests(Requests::UpTo(std::num::NonZeroU8::MAX))
        .ping_interval(Timer::of(hour))
        .liveness_timeout(Timer::of(hour));
    // Every retransmission is lost as well, so the gap stays open.
    let mut sim = lossy_sim_with(host, no_fec(), std::iter::once(2).chain(4..60).collect());
    for n in (60..63).map(note_on) {
        sim.client.send(sim.now, &[n]).unwrap();
        sim.flush();
    }
    let requests = |sim: &Sim| {
        sim.commands(Direction::HostToClient)
            .iter()
            .filter(|c| matches!(c, Command::RetransmitRequest(_)))
            .count()
    };
    assert_eq!(requests(&sim), 0, "the first wait is an hour");
    sim.advance(hour);
    assert_eq!(requests(&sim), 1);
    // The second wait would be two hours doubled; the ceiling holds it to one.
    sim.advance(hour);
    assert_eq!(requests(&sim), 2);
    sim.advance(hour);
    assert_eq!(requests(&sim), 3);
}

#[test]
fn a_gap_that_is_never_filled_is_given_up_after_the_requests_are_spent() {
    // Every retransmission is lost as well.
    let mut sim = lossy_sim(no_fec(), std::iter::once(2).chain(4..60).collect());
    let notes: Vec<_> = (60..63).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(Duration::from_secs(2));
    assert!(sim.host_events.contains(&Event::DataLost {
        peer: client_addr(),
        first: SequenceNumber::new(1),
        count: 1
    }));
    assert_eq!(Sim::umps(&sim.host_events), vec![notes[0], notes[2]]);
}

/// Spec: m2-124/7.2.2#3
#[test]
fn duplicated_datagrams_deliver_each_ump_once() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::ClientToHost {
            Fate::Twice
        } else {
            Fate::Deliver
        }
    });
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    assert_eq!(Sim::umps(&sim.host_events), notes);
}

#[test]
fn commands_arriving_out_of_order_are_delivered_in_order() {
    let mut sim = Sim::new(
        HostConfig::new(identity("Studio", "HOST-1")),
        Credentials::none(),
        no_fec(),
    );
    sim.connect();
    sim.host_events.clear();
    let notes: Vec<_> = (60..63).map(note_on).collect();
    let mut datagrams = Vec::new();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        datagrams.push(sim.client.poll_transmit().unwrap());
    }
    for i in [2, 0, 1] {
        sim.host
            .on_message(client_addr(), &datagrams[i].payload, sim.now);
    }
    sim.flush();
    assert_eq!(Sim::umps(&sim.host_events), notes);
}

#[test]
fn sequence_numbers_wrap_through_ffff_without_losing_or_repeating_a_command() {
    let mut sim = established();
    let total = 65_540usize;
    for i in 0..total {
        sim.client
            .send(sim.now, &[note_on(u8::try_from(i % 128).unwrap())])
            .unwrap();
        sim.flush();
    }
    let umps = Sim::umps(&sim.host_events);
    assert_eq!(umps.len(), total);
    assert!(
        umps.iter()
            .enumerate()
            .all(|(i, u)| *u == note_on(u8::try_from(i % 128).unwrap()))
    );
    let (_, last) = sim.log.last().unwrap();
    let last_seq = tutti_midi_network::parse(last)
        .unwrap()
        .filter_map(|c| match c.unwrap() {
            Command::UmpData(d) => Some(d.sequence()),
            _ => None,
        })
        .last()
        .unwrap();
    assert_eq!(last_seq, SequenceNumber::new(3));
}

/// Spec: m2-124/5.1.1#1, m2-124/7.1#7
#[test]
fn a_burst_larger_than_one_command_is_split_and_no_datagram_exceeds_1400_bytes() {
    let mut sim = established();
    let packets: Vec<_> = (0..100)
        .map(|i| tutti_ump::Ump([0x5000_0000 | i, 1, 2, 3]))
        .collect();
    sim.client.send(sim.now, &packets).unwrap();
    sim.flush();
    assert_eq!(Sim::umps(&sim.host_events), packets);
    assert!(sim.log.iter().all(|(_, b)| b.len() <= 1400));
    assert!(sim.log.len() >= 2, "the burst took more than one datagram");
    assert!(
        data_commands(&sim, Direction::ClientToHost)
            .iter()
            .all(|d| d.words().len() <= 64)
    );
}

#[test]
fn sending_before_a_session_is_established_fails_with_the_state() {
    let mut sim = Sim::open();
    assert_eq!(
        sim.client.send(sim.now, &[note_on(60)]),
        Err(SendError::NotEstablished {
            state: SessionState::Idle
        })
    );
    assert_eq!(
        sim.host.send(sim.now, client_addr(), &[note_on(60)]),
        Err(SendError::NotEstablished {
            state: SessionState::Idle
        })
    );
}

/// Spec: m2-124/7.1#1, m2-124/6.5#2
#[test]
fn the_host_sends_to_the_client_too() {
    let mut sim = established();
    sim.host
        .send(sim.now, client_addr(), &[note_on(72)])
        .unwrap();
    sim.flush();
    assert_eq!(Sim::umps(&sim.client_events), vec![note_on(72)]);
}

/// An edge counts what it loses: an application that never polls events keeps
/// the first [`MAX_QUEUED_EVENTS`] and the count of the rest.
#[test]
fn a_full_event_queue_keeps_what_it_holds_and_counts_the_rest() {
    // A Client whose events nobody polls: datagrams cross by hand.
    let now = Instant::now();
    let mut host = Host::new(HostConfig::new(identity("Studio", "HOST-1")));
    let mut client = Client::new(
        identity("Laptop", "CLIENT-1"),
        Credentials::none(),
        SessionConfig::default(),
        host_addr(),
        now,
    );
    let deliver = |host: &mut Host, client: &mut Client, now| loop {
        let mut moved = false;
        while let Some(t) = client.poll_transmit() {
            moved = true;
            host.on_message(client_addr(), &t.payload, now);
        }
        while let Some(t) = host.poll_transmit() {
            moved = true;
            client.on_message(host_addr(), &t.payload, now);
        }
        if !moved {
            break;
        }
    };
    client.connect(now);
    deliver(&mut host, &mut client, now);
    assert_eq!(client.state(), SessionState::Established);
    let batches = 300;
    let per_batch = 16;
    for batch in 0..batches {
        // The first batch is the oldest data in the queue.
        let note = if batch == 0 { 100 } else { 60 };
        host.send(now, client_addr(), &[note_on(note); 16]).unwrap();
        deliver(&mut host, &mut client, now);
    }
    // One Established event, then one per UMP delivered; the queue holds the
    // first MAX_QUEUED_EVENTS.
    let total = 1 + batches * per_batch;
    assert!(total > MAX_QUEUED_EVENTS);
    let lost = Losses::lost((total - MAX_QUEUED_EVENTS) as u64);
    assert_eq!(client.losses(), lost);
    // A Closed at a full queue is delivered and evicts the oldest data event.
    host.close(now, client_addr(), ByeReason::PowerDown);
    deliver(&mut host, &mut client, now);
    assert_eq!(client.losses(), lost + Losses::evicted(1));
    let mut held = 0;
    let mut first = None;
    let mut last = None;
    let mut oldest_notes = 0;
    while let Some(event) = client.poll_event() {
        if event
            == (Event::Ump {
                peer: host_addr(),
                packet: note_on(100),
            })
        {
            oldest_notes += 1;
        }
        first.get_or_insert_with(|| event.clone());
        last = Some(event);
        held += 1;
    }
    assert_eq!(held, MAX_QUEUED_EVENTS, "the queue never exceeds the cap");
    assert_eq!(oldest_notes, per_batch - 1, "the oldest data event is gone");
    assert!(matches!(first, Some(Event::Established { .. })));
    assert!(matches!(
        last,
        Some(Event::Closed {
            reason: ByeReason::PowerDown,
            ..
        })
    ));
}

/// A queue full of session-life events loses its oldest to a newer one.
#[test]
fn a_queue_of_session_events_evicts_its_oldest_for_the_newest() {
    let now = Instant::now();
    let mut host = Host::new(HostConfig::new(identity("Studio", "HOST-1")));
    let mut client = Client::new(
        identity("Laptop", "CLIENT-1"),
        Credentials::none(),
        SessionConfig::default(),
        host_addr(),
        now,
    );
    let deliver = |host: &mut Host, client: &mut Client| loop {
        let mut moved = false;
        while let Some(t) = client.poll_transmit() {
            moved = true;
            host.on_message(client_addr(), &t.payload, now);
        }
        while let Some(t) = host.poll_transmit() {
            moved = true;
            client.on_message(host_addr(), &t.payload, now);
        }
        if !moved {
            break;
        }
    };
    // Each cycle is an Established and a Closed.
    for _ in 0..MAX_QUEUED_EVENTS / 2 {
        client.connect(now);
        deliver(&mut host, &mut client);
        host.close(now, client_addr(), ByeReason::UserTerminated);
        deliver(&mut host, &mut client);
    }
    assert_eq!(client.state(), SessionState::Idle);
    assert_eq!(client.losses(), Losses::NONE);
    // One more session-life event: the oldest (the first Established) goes.
    client.connect(now);
    deliver(&mut host, &mut client);
    assert_eq!(client.losses(), Losses::evicted(1));
    let mut events = Vec::new();
    while let Some(event) = client.poll_event() {
        events.push(event);
    }
    assert_eq!(events.len(), MAX_QUEUED_EVENTS);
    assert!(matches!(events[0], Event::Closed { .. }));
    assert!(matches!(
        events[MAX_QUEUED_EVENTS - 1],
        Event::Established { .. }
    ));
}

// ---- Idle periods ----

fn zero_length(sim: &Sim) -> usize {
    data_commands(sim, Direction::ClientToHost)
        .iter()
        .filter(|d| d.words().is_empty())
        .map(|d| d.sequence())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Spec: m2-124/7.2.1#1, m2-124/7.2.1#7
#[test]
fn an_idle_period_is_declared_with_zero_length_commands_that_use_sequence_numbers() {
    let mut sim = established();
    sim.client.send(sim.now, &[note_on(60)]).unwrap();
    sim.flush();
    assert_eq!(zero_length(&sim), 0);
    sim.advance(ms(10));
    assert_eq!(zero_length(&sim), 1);
    // The two FEC repeats follow at the same interval.
    sim.advance(ms(20));
    assert_eq!(zero_length(&sim), 3);
    let mut idle: Vec<_> = data_commands(&sim, Direction::ClientToHost)
        .iter()
        .filter(|d| d.words().is_empty())
        .map(|d| d.sequence().get())
        .collect();
    idle.sort_unstable();
    idle.dedup();
    assert_eq!(
        idle,
        [1, 2, 3],
        "each zero-length command takes the next number after the note's 0"
    );
    assert!(
        sim.host_events
            .iter()
            .all(|e| !matches!(e, Event::Ump { packet, .. } if *packet != note_on(60)))
    );
}

/// Spec: m2-124/7.2.1#6
#[test]
fn zero_length_commands_slow_down_and_then_stop() {
    let mut sim = established();
    sim.client.send(sim.now, &[note_on(60)]).unwrap();
    sim.flush();
    sim.advance(Duration::from_secs(10));
    let after_ten = zero_length(&sim);
    assert!(after_ten > 3 && after_ten < 20, "{after_ten}");
    sim.advance(Duration::from_secs(10));
    assert_eq!(
        zero_length(&sim),
        after_ten,
        "no more once the interval is long"
    );
}

/// Spec: m2-124/7.2.1#2
#[test]
fn the_first_zero_length_command_comes_within_300_ms_at_the_longest_idle_interval() {
    let mut sim = Sim::new(
        HostConfig::new(identity("Studio", "HOST-1")),
        Credentials::none(),
        SessionConfig::default().idle_interval(IdleInterval::of(ms(300))),
    );
    sim.connect();
    sim.log.clear();
    sim.client.send(sim.now, &[note_on(60)]).unwrap();
    sim.flush();
    sim.advance(ms(299));
    assert_eq!(zero_length(&sim), 0);
    sim.advance(ms(1));
    assert_eq!(zero_length(&sim), 1);
}

// ---- Keep-alive ----

#[test]
fn quiet_peers_ping_each_other_and_keep_the_session_alive() {
    let mut sim = established();
    sim.advance(Duration::from_secs(30));
    assert_eq!(sim.client.state(), SessionState::Established);
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
    let pings: Vec<PingId> = sim
        .commands(Direction::ClientToHost)
        .into_iter()
        .filter_map(|c| match c {
            Command::Ping(id) => Some(id),
            _ => None,
        })
        .collect();
    assert!(pings.len() >= 5, "{pings:?}");
    let replies: Vec<PingId> = sim
        .commands(Direction::HostToClient)
        .into_iter()
        .filter_map(|c| match c {
            Command::PingReply(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        &replies[..pings.len().min(replies.len())],
        &pings[..pings.len().min(replies.len())]
    );
}

/// Spec: m2-124/6.14#1, m2-124/6.14#4
#[test]
fn a_ping_gets_a_reply_with_the_same_id_in_any_state() {
    let mut sim = Sim::open();
    sim.host.on_message(
        client_addr(),
        &datagram(&Command::Ping(PingId::new(0xCAFE))),
        sim.now,
    );
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::PingReply(PingId::new(0xCAFE))]
    );
}

/// Spec: m2-124/6.14#3
#[test]
fn a_ping_reply_nobody_asked_for_gets_a_bad_ping_reply_nak() {
    let mut sim = established();
    sim.host.on_message(
        client_addr(),
        &datagram(&Command::PingReply(PingId::new(77))),
        sim.now,
    );
    sim.flush();
    assert!(
        sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(
                c,
                Command::Nak(n) if n.reason == NakReason::BadPingReply
            ))
    );
}

/// Spec: m2-124/6.13#2, m2-124/6.13#3, m2-124/6.16#2
#[test]
fn a_silent_peer_ends_the_session_with_a_timeout_bye_that_is_repeated() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::HostToClient {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.advance(Duration::from_secs(20));
    let byes = sim
        .commands(Direction::ClientToHost)
        .into_iter()
        .filter(|c| *c == Command::Bye(Box::new(Bye::new(ByeReason::Timeout))))
        .count();
    assert!(
        byes >= 2,
        "the Bye is repeated until answered or given up: {byes}"
    );
    assert!(sim.host_events.contains(&Event::Closed {
        peer: client_addr(),
        ended_by: EndedBy::Remote,
        reason: ByeReason::Timeout
    }));
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        ended_by: EndedBy::Local,
        reason: ByeReason::Timeout
    }));
    assert_eq!(sim.client.state(), SessionState::Idle);
}

// ---- Bye ----

/// Spec: m2-124/6.16#3, m2-124/6.17#1, m2-124/6.17#2
#[test]
fn a_bye_is_answered_once_and_both_ends_return_to_idle() {
    let mut sim = established();
    sim.client.close(sim.now, ByeReason::UserTerminated);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
    assert!(sim.host_events.contains(&Event::Closed {
        peer: client_addr(),
        ended_by: EndedBy::Remote,
        reason: ByeReason::UserTerminated
    }));
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        ended_by: EndedBy::Local,
        reason: ByeReason::UserTerminated
    }));
    let replies = sim
        .commands(Direction::HostToClient)
        .iter()
        .filter(|c| **c == Command::ByeReply)
        .count();
    assert_eq!(replies, 1);
    assert_eq!(
        sim.client.send(sim.now, &[note_on(1)]),
        Err(SendError::NotEstablished {
            state: SessionState::Idle
        })
    );
    // The Bye Reply stopped the repeats: nothing more is sent, ever.
    sim.log.clear();
    sim.advance(Duration::from_secs(60));
    assert_eq!(sim.log, vec![]);
}

/// A datagram holding one Bye (reason User Terminated) whose message field is
/// `text`, padded with zero to a word.
fn raw_bye(text: &[u8]) -> Vec<u8> {
    let words = text.len().div_ceil(4);
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0xF0, u8::try_from(words).unwrap(), 0x01, 0x00]);
    bytes.extend_from_slice(text);
    bytes.resize(8 + words * 4, 0);
    bytes
}

/// Table 26: a Bye's payload length is 0..255 words, so its message is up to 1020 bytes.
/// Spec: m2-124/6.16#1, m2-124/6.16#5
#[test]
fn a_bye_with_the_longest_message_the_wire_allows_ends_the_session() {
    let mut sim = established();
    sim.client
        .on_message(host_addr(), &raw_bye(&[b'x'; 1020]), sim.now);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        ended_by: EndedBy::Remote,
        reason: ByeReason::UserTerminated
    }));
    assert!(
        sim.commands(Direction::ClientToHost)
            .contains(&Command::ByeReply)
    );
}

/// The message is advisory: a message that is not UTF-8 does not keep the Session open.
/// Spec: m2-124/6.16#1, m2-124/6.16#5
#[test]
fn a_bye_whose_message_is_not_utf8_still_ends_the_session() {
    let mut sim = established();
    sim.client
        .on_message(host_addr(), &raw_bye(b"ok\xFF\xFEtail"), sim.now);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        ended_by: EndedBy::Remote,
        reason: ByeReason::UserTerminated
    }));
    assert!(
        sim.commands(Direction::ClientToHost)
            .contains(&Command::ByeReply)
    );
}

/// Spec: m2-124/6.16#4
#[test]
fn a_bye_from_a_peer_in_no_session_is_still_answered() {
    let mut sim = Sim::open();
    sim.host.on_message(
        client_addr(),
        &datagram(&Command::Bye(Box::new(Bye::new(ByeReason::UserTerminated)))),
        sim.now,
    );
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::ByeReply]
    );
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

#[test]
fn a_host_that_powers_down_ends_every_session() {
    let mut sim = established();
    sim.host.close_all(sim.now);
    sim.flush();
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        ended_by: EndedBy::Remote,
        reason: ByeReason::PowerDown
    }));
}

/// Spec: m2-124/7.1#2
#[test]
fn data_outside_a_session_gets_a_bye_for_session_not_established() {
    let mut sim = Sim::open();
    let data = Command::UmpData(UmpData::new(SequenceNumber::ZERO, &[note_on(60)]).unwrap());
    sim.host
        .on_message(client_addr(), &datagram(&data), sim.now);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Box::new(Bye::new(
            ByeReason::SessionNotEstablished
        )))]
    );
    assert!(Sim::umps(&sim.host_events).is_empty());
}

// ---- Session reset ----

/// Spec: m2-124/6.11#4, m2-124/6.11#8, m2-124/6.11#10, m2-124/6.11#11, m2-124/6.11#12, m2-124/6.12#1, m2-124/6.12#2
#[test]
fn a_reset_zeroes_both_sequence_numbers_and_flushes_the_buffers() {
    let mut sim = established();
    for n in 60..63 {
        sim.client.send(sim.now, &[note_on(n)]).unwrap();
        sim.flush();
    }
    sim.client.reset(sim.now);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert!(
        sim.client_events
            .contains(&Event::Reset { peer: host_addr() })
    );
    assert!(sim.host_events.contains(&Event::Reset {
        peer: client_addr()
    }));
    sim.log.clear();
    sim.host_events.clear();
    // The Retransmit buffers are empty: each end answers a request for the
    // commands it sent before the reset with an Error and no data.
    let request = datagram(&Command::RetransmitRequest(RetransmitRequest {
        first: SequenceNumber::ZERO,
        count: 0,
    }));
    sim.client.on_message(host_addr(), &request, sim.now);
    sim.host.on_message(client_addr(), &request, sim.now);
    sim.flush();
    for dir in [Direction::ClientToHost, Direction::HostToClient] {
        let sent = sim.commands(dir);
        assert!(
            matches!(&sent[..], [Command::RetransmitError(_)]),
            "{dir:?}: {sent:?}"
        );
    }
    sim.log.clear();
    sim.host_events.clear();
    sim.client.send(sim.now, &[note_on(100)]).unwrap();
    sim.flush();
    let sent = data_commands(&sim, Direction::ClientToHost);
    assert_eq!(
        sent.len(),
        1,
        "the flushed buffer leaves no FEC repeat to send"
    );
    assert_eq!(sent[0].sequence(), SequenceNumber::ZERO);
    assert_eq!(Sim::umps(&sim.host_events), vec![note_on(100)]);
    // The host's side restarts at 0 too, and the client takes it as the next.
    sim.client_events.clear();
    sim.host
        .send(sim.now, client_addr(), &[note_on(101)])
        .unwrap();
    sim.flush();
    let back = data_commands(&sim, Direction::HostToClient);
    assert_eq!(back.len(), 1, "the host's flushed buffer leaves no repeat");
    assert_eq!(back[0].sequence(), SequenceNumber::ZERO);
    assert_eq!(Sim::umps(&sim.client_events), vec![note_on(101)]);
}

/// Spec: m2-124/6.11#2, m2-124/6.11#3, m2-124/6.11#6
#[test]
fn no_data_is_sent_while_a_reset_is_pending_and_the_reset_is_repeated() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::HostToClient {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.client.reset(sim.now);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::PendingSessionReset);
    assert_eq!(
        sim.client.send(sim.now, &[note_on(1)]),
        Err(SendError::NotEstablished {
            state: SessionState::PendingSessionReset
        })
    );
    sim.advance(ms(1200));
    let resets = sim
        .commands(Direction::ClientToHost)
        .iter()
        .filter(|c| **c == Command::SessionReset)
        .count();
    assert!(resets >= 3, "{resets}");
}

/// Spec: m2-124/6.11#5, m2-124/6.11#7
#[test]
fn an_unanswered_reset_ends_the_session_with_a_timeout_bye() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::HostToClient {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.client.reset(sim.now);
    sim.advance(Duration::from_secs(6));
    assert!(
        sim.commands(Direction::ClientToHost)
            .contains(&Command::Bye(Box::new(Bye::new(ByeReason::Timeout))))
    );
}

/// Spec: m2-124/6.11#9
#[test]
fn a_reset_outside_a_session_gets_a_bye_for_session_not_established() {
    let mut sim = Sim::open();
    sim.host
        .on_message(client_addr(), &datagram(&Command::SessionReset), sim.now);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Box::new(Bye::new(
            ByeReason::SessionNotEstablished
        )))]
    );
}

// ---- Unreadable and unknown commands ----

/// Spec: m2-124/5.5#1
#[test]
fn an_unknown_command_gets_a_nak_for_command_not_supported() {
    let mut sim = established();
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0x55, 0x00, 0x00, 0x00]);
    sim.host.on_message(client_addr(), &bytes, sim.now);
    sim.flush();
    let [Command::Nak(nak)] = &sim.commands(Direction::HostToClient)[..] else {
        panic!()
    };
    assert_eq!(nak.reason, NakReason::NotSupported);
    assert_eq!(nak.refused.word(), 0x5500_0000);
    assert_eq!(
        sim.host.state(client_addr()),
        SessionState::Established,
        "a NAK ends no session"
    );
}

#[test]
fn a_malformed_command_gets_a_nak_for_command_malformed() {
    let mut sim = established();
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]);
    sim.host.on_message(client_addr(), &bytes, sim.now);
    sim.flush();
    assert!(matches!(
        &sim.commands(Direction::HostToClient)[..],
        [Command::Nak(n)] if n.reason == NakReason::Malformed
    ));
}

/// Spec: m2-124/5.2#3
#[test]
fn a_datagram_without_the_signature_changes_nothing() {
    let mut sim = established();
    sim.host
        .on_message(client_addr(), b"XXXX\xF0\x00\x01\x00", sim.now);
    sim.flush();
    assert!(sim.log.is_empty());
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

#[test]
fn a_client_ignores_datagrams_from_any_address_but_its_host() {
    let mut sim = established();
    let stranger = "10.0.0.77:1".parse().unwrap();
    sim.client.on_message(
        stranger,
        &datagram(&Command::Bye(Box::new(Bye::new(ByeReason::UserTerminated)))),
        sim.now,
    );
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Established);
}

#[test]
fn a_stranger_that_is_not_inviting_is_answered_statelessly_and_costs_the_host_no_entry() {
    let mut sim = Sim::open();
    let stranger = "10.0.0.50:9000".parse().unwrap();
    let replies = |sim: &mut Sim, bytes: &[u8]| {
        sim.host.on_message(stranger, bytes, sim.now);
        let mut out = Vec::new();
        while let Some(t) = sim.host.poll_transmit() {
            assert_eq!(t.to, stranger);
            out.extend(
                tutti_midi_network::parse(&t.payload)
                    .unwrap()
                    .map(Result::unwrap),
            );
        }
        out
    };
    let data = Command::UmpData(UmpData::new(SequenceNumber::ZERO, &[note_on(60)]).unwrap());
    assert_eq!(
        replies(&mut sim, &datagram(&data)),
        vec![Command::Bye(Box::new(Bye::new(
            ByeReason::SessionNotEstablished
        )))]
    );
    assert_eq!(sim.host.session_count(), 0);
    assert_eq!(
        replies(&mut sim, &datagram(&Command::Ping(PingId::new(5)))),
        vec![Command::PingReply(PingId::new(5))]
    );
    assert_eq!(sim.host.session_count(), 0);
    assert_eq!(
        replies(&mut sim, &datagram(&Command::SessionReset)),
        vec![Command::Bye(Box::new(Bye::new(
            ByeReason::SessionNotEstablished
        )))]
    );
    assert_eq!(sim.host.session_count(), 0);
    assert_eq!(sim.host.state(stranger), SessionState::Idle);
}

#[test]
fn only_an_invitation_below_capacity_creates_a_host_entry() {
    let mut sim = Sim::open();
    sim.connect();
    assert_eq!(sim.host.session_count(), 1);
}

#[test]
fn a_receiver_that_never_requests_gives_a_gap_up_without_asking() {
    let mut host = HostConfig::new(identity("Studio", "HOST-1"));
    host.session = SessionConfig::default().requests(Requests::Never);
    let mut sim = Sim::new(host, Credentials::none(), no_fec());
    sim.connect();
    sim.host_events.clear();
    let notes: Vec<_> = (60..63).map(note_on).collect();
    let mut datagrams = Vec::new();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        datagrams.push(sim.client.poll_transmit().unwrap());
    }
    for i in [0, 2] {
        sim.host
            .on_message(client_addr(), &datagrams[i].payload, sim.now);
    }
    sim.advance(ms(10));
    assert!(sim.host_events.contains(&Event::DataLost {
        peer: client_addr(),
        first: SequenceNumber::new(1),
        count: 1
    }));
    assert!(
        !sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(c, Command::RetransmitRequest(_)))
    );
}

// ---- Commands outside the state they belong to ----

/// Spec: m2-124/6.5#3, m2-124/6.6#3
#[test]
fn a_late_accepted_or_pending_is_ignored_by_an_established_client() {
    let mut sim = established();
    for reply in [
        Command::Accepted(identity("Impostor", "HOST-2")),
        Command::Pending(identity("Impostor", "HOST-2")),
    ] {
        sim.client
            .on_message(host_addr(), &datagram(&reply), sim.now);
        sim.flush();
    }
    assert_eq!(sim.log, vec![], "no reply to either");
    assert!(sim.client_events.is_empty());
    assert_eq!(sim.client.state(), SessionState::Established);
    assert_eq!(sim.client.remote(), Some(identity("Studio", "HOST-1")));
}

/// Spec: m2-124/6.6#3
#[test]
fn a_pending_reply_to_a_client_with_no_invitation_gets_a_bye_for_no_pending_invitation() {
    let mut sim = Sim::open();
    let pending = datagram(&Command::Pending(identity("Studio", "HOST-1")));
    sim.client.on_message(host_addr(), &pending, sim.now);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::ClientToHost),
        vec![Command::Bye(Box::new(Bye::new(
            ByeReason::NoPendingInvitation
        )))]
    );
}

/// A Host is not the one a Client's replies are meant for: each reply it
/// receives is refused as not expected, quoting the header, and no Session is
/// harmed.
/// Spec: m2-124/6.6#4, m2-124/6.7#9, m2-124/6.8#7
#[test]
fn a_host_naks_a_pending_or_a_challenge_as_not_expected() {
    let mut sim = established();
    for command in [
        Command::Pending(identity("Laptop", "CLIENT-1")),
        auth_required(),
        user_auth_required(),
    ] {
        sim.log.clear();
        sim.host
            .on_message(client_addr(), &datagram(&command), sim.now);
        sim.flush();
        let [Command::Nak(nak)] = &sim.commands(Direction::HostToClient)[..] else {
            panic!("expected one NAK for {command:?}")
        };
        assert_eq!(nak.reason, NakReason::NotExpected);
        assert_eq!(nak.refused, command.header());
        assert_eq!(sim.host.state(client_addr()), SessionState::Established);
    }
}

/// Spec: m2-124/6.17#3
#[test]
fn a_bye_reply_outside_an_established_session_is_ignored() {
    let mut sim = Sim::open();
    sim.client
        .on_message(host_addr(), &datagram(&Command::ByeReply), sim.now);
    sim.host
        .on_message(client_addr(), &datagram(&Command::ByeReply), sim.now);
    sim.flush();
    assert_eq!(sim.log, vec![]);
    assert!(sim.client_events.is_empty() && sim.host_events.is_empty());
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

/// Spec: m2-124/6.12#4, m2-124/7.2.3#12, m2-124/7.2.4#4
#[test]
fn a_reset_reply_a_retransmit_request_or_error_outside_a_session_gets_a_bye() {
    let commands = [
        Command::SessionResetReply,
        Command::RetransmitRequest(RetransmitRequest {
            first: SequenceNumber::ZERO,
            count: 1,
        }),
        Command::RetransmitError(RetransmitRefusal {
            reason: RetransmitErrorReason::BufferMiss,
            first: SequenceNumber::new(4),
        }),
    ];
    for command in commands {
        // To a Host that holds no session for the peer, and to a Client that
        // has sent no Invitation.
        let mut sim = Sim::open();
        sim.host
            .on_message(client_addr(), &datagram(&command), sim.now);
        sim.client
            .on_message(host_addr(), &datagram(&command), sim.now);
        sim.flush();
        // Each Bye reaches the other end, which answers it; the Byes are
        // what the commands themselves drew.
        let bye = Command::Bye(Box::new(Bye::new(ByeReason::SessionNotEstablished)));
        for dir in [Direction::HostToClient, Direction::ClientToHost] {
            let sent = sim.commands(dir);
            assert_eq!(sent.first(), Some(&bye), "{dir:?} {command:?}");
            assert!(!sent.iter().any(|c| matches!(c, Command::Nak(_))));
        }
    }
}
