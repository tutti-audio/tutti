//! Properties over arbitrary input: the readers never panic, a command written
//! is a command read, and an endpoint fed any datagrams answers only with
//! datagrams a peer can read.

use std::net::SocketAddr;
use std::time::Duration;
use tutti_platform::Instant;

use proptest::prelude::*;
use tutti_midi_network::{
    Admission, AuthChallenge, AuthMethods, AuthState, Bye, ByeReason, ByeText, Client, Command,
    Credentials, DatagramBuilder, Digest, EndpointIdentity, Header, Host, HostConfig, Invitation,
    Nak, NakReason, NakText, NetworkEndpoint, Nonce, ParseError, PingId, RetransmitErrorReason,
    RetransmitRefusal, RetransmitRequest, SIGNATURE, SequenceNumber, SessionConfig, UmpData,
    UserAuthChallenge, UserAuthState, UserAuthentication, Username, parse,
};
use tutti_ump::Ump;

use crate::sim::identity;

fn identity_strategy() -> impl Strategy<Value = EndpointIdentity> {
    ("[A-Za-z0-9 ]{1,40}", "[A-Za-z0-9-]{1,40}").prop_map(|(name, id)| identity(&name, &id))
}

fn nonce_strategy() -> impl Strategy<Value = Nonce> {
    proptest::array::uniform16(32u8..=126).prop_map(|bytes| Nonce::new(bytes).unwrap())
}

fn digest_strategy() -> impl Strategy<Value = Digest> {
    proptest::array::uniform32(any::<u8>()).prop_map(Digest::new)
}

fn sequence() -> impl Strategy<Value = SequenceNumber> {
    any::<u16>().prop_map(SequenceNumber::new)
}

/// Whole UMPs: note-ons of the 64-bit message type.
fn packets() -> impl Strategy<Value = Vec<Ump>> {
    proptest::collection::vec(any::<u8>(), 0..=32).prop_map(|notes| {
        notes
            .into_iter()
            .map(|n| Ump([0x4090_0000 | (u32::from(n & 0x7F) << 8), 0x8000_0000, 0, 0]))
            .collect()
    })
}

/// Every NAK reason, the reserved codes as the reader keeps them.
fn nak_reason() -> impl Strategy<Value = NakReason> {
    prop_oneof![
        Just(NakReason::Other),
        Just(NakReason::NotSupported),
        Just(NakReason::NotExpected),
        Just(NakReason::Malformed),
        Just(NakReason::BadPingReply),
        (4u8..0x20).prop_map(NakReason::Reserved),
        (0x21u8..=0xFF).prop_map(NakReason::Reserved),
    ]
}

fn retransmit_error_reason() -> impl Strategy<Value = RetransmitErrorReason> {
    prop_oneof![
        Just(RetransmitErrorReason::Unknown),
        Just(RetransmitErrorReason::BufferMiss),
        (2u8..=0xFF).prop_map(RetransmitErrorReason::Reserved),
    ]
}

fn command() -> impl Strategy<Value = Command> {
    let invitation = (identity_strategy(), 0u8..4).prop_map(|(identity, methods)| {
        Command::Invitation(Invitation {
            methods: AuthMethods::from_bits(methods),
            identity,
        })
    });
    let auth_required = (
        prop_oneof![Just(AuthState::First), Just(AuthState::DigestWrong)],
        nonce_strategy(),
        identity_strategy(),
    )
        .prop_map(|(state, nonce, identity)| {
            Command::AuthRequired(Box::new(AuthChallenge {
                state,
                nonce,
                identity,
            }))
        });
    let user_auth_required = (
        prop_oneof![
            Just(UserAuthState::First),
            Just(UserAuthState::DigestWrong),
            Just(UserAuthState::UserNotFound)
        ],
        nonce_strategy(),
        identity_strategy(),
    )
        .prop_map(|(state, nonce, identity)| {
            Command::UserAuthRequired(Box::new(UserAuthChallenge {
                state,
                nonce,
                identity,
            }))
        });
    let user_auth = (digest_strategy(), "[a-z]{1,40}").prop_map(|(digest, name)| {
        Command::InvitationWithUserAuth(Box::new(UserAuthentication {
            digest,
            username: Username::new(&name).unwrap(),
        }))
    });
    let nak = (nak_reason(), any::<u32>(), "[a-z ]{0,60}").prop_map(|(reason, refused, text)| {
        Command::Nak(Box::new(Nak {
            reason,
            refused: Header::from_word(refused),
            text: NakText::new(&text).unwrap(),
        }))
    });
    let bye = (any::<u8>(), "[a-z ]{0,60}").prop_map(|(reason, text)| {
        Command::Bye(Box::new(Bye {
            reason: ByeReason::from_code(reason),
            text: ByeText::new(&text).unwrap(),
        }))
    });
    let data = (sequence(), packets())
        .prop_map(|(seq, packets)| Command::UmpData(UmpData::new(seq, &packets).unwrap()));
    prop_oneof![
        invitation,
        digest_strategy().prop_map(Command::InvitationWithAuth),
        user_auth,
        identity_strategy().prop_map(Command::Accepted),
        identity_strategy().prop_map(Command::Pending),
        auth_required,
        user_auth_required,
        any::<u32>().prop_map(|id| Command::Ping(PingId::new(id))),
        any::<u32>().prop_map(|id| Command::PingReply(PingId::new(id))),
        (sequence(), any::<u16>()).prop_map(|(first, count)| {
            Command::RetransmitRequest(RetransmitRequest { first, count })
        }),
        (retransmit_error_reason(), sequence()).prop_map(|(reason, first)| {
            Command::RetransmitError(RetransmitRefusal { reason, first })
        }),
        Just(Command::SessionReset),
        Just(Command::SessionResetReply),
        nak,
        bye,
        Just(Command::ByeReply),
        data,
    ]
}

/// The padding of the string field of `len` bytes at `start`: the bytes that
/// complete its last word, and where the next field begins. An empty string
/// takes one word when the field is required and none when it is optional.
fn padding(start: usize, len: usize, required: bool) -> (std::ops::Range<usize>, usize) {
    let words = if required {
        len.div_ceil(4).max(1)
    } else {
        len.div_ceil(4)
    };
    (start + len..start + words * 4, start + words * 4)
}

/// One reserved range.
fn at(range: std::ops::Range<usize>) -> Vec<std::ops::Range<usize>> {
    vec![range]
}

/// The padding of a name field and the Product Instance Id field after it.
fn identity_padding(start: usize, identity: &EndpointIdentity) -> Vec<std::ops::Range<usize>> {
    let (name, next) = padding(start, identity.name().as_bytes().len(), true);
    let (id, _) = padding(next, identity.instance_id().as_str().len(), true);
    vec![name, id]
}

/// The bytes of `command` as written that M2-124 Tables 8 to 34 leave reserved
/// or unused, and so section 5.3 and 5.4 require to be zero. A Command
/// Specific Data byte that carries a value is not listed.
fn reserved_bytes(command: &Command) -> Vec<std::ops::Range<usize>> {
    match command {
        Command::Invitation(inv) => identity_padding(4, &inv.identity),
        Command::InvitationWithAuth(_)
        | Command::Ping(_)
        | Command::PingReply(_)
        | Command::SessionReset
        | Command::SessionResetReply
        | Command::ByeReply => at(2..4),
        Command::InvitationWithUserAuth(auth) => {
            let mut ranges = at(2..4);
            ranges.push(padding(4 + 32, auth.username.as_str().len(), true).0);
            ranges
        }
        Command::Accepted(identity) | Command::Pending(identity) => {
            let mut ranges = at(3..4);
            ranges.extend(identity_padding(4, identity));
            ranges
        }
        Command::AuthRequired(c) => identity_padding(4 + 16, &c.identity),
        Command::UserAuthRequired(c) => identity_padding(4 + 16, &c.identity),
        // The payload word is the count and 16 reserved bits.
        Command::RetransmitRequest(_) => at(6..8),
        Command::RetransmitError(_) => vec![3..4, 6..8],
        Command::Nak(nak) => vec![3..4, padding(8, nak.text.as_str().len(), false).0],
        Command::Bye(bye) => vec![3..4, padding(4, bye.text.as_str().len(), false).0],
        Command::UmpData(_) => vec![],
    }
}

fn signed(bytes: &[u8]) -> Vec<u8> {
    let mut all = SIGNATURE.to_be_bytes().to_vec();
    all.extend_from_slice(bytes);
    all
}

proptest! {
    /// Any bytes, with and without the signature in front, are read or refused
    /// by name, never a panic; a command that is read writes back to a datagram
    /// that reads as the same command.
    #[test]
    fn arbitrary_bytes_through_the_datagram_reader_never_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..400),
    ) {
        for datagram in [bytes.clone(), signed(&bytes)] {
            let Ok(commands) = parse(&datagram) else { continue };
            for command in commands.flatten() {
                let mut builder = DatagramBuilder::new();
                builder.push(&command);
                let again: Vec<_> = parse(&builder.finish()).unwrap().collect();
                prop_assert_eq!(again, vec![Ok(command)]);
            }
        }
    }

    /// A header with any payload, of any length, is read or refused by name.
    #[test]
    fn an_arbitrary_header_and_payload_never_panic_the_command_reader(
        word in any::<u32>(),
        payload in proptest::collection::vec(any::<u8>(), 0..1100),
    ) {
        let header = Header::from_word(word);
        let result = Command::parse(header, &payload);
        if payload.len() != usize::from(header.payload_words()) * 4 {
            prop_assert_eq!(
                result,
                Err(ParseError::Length {
                    code: header.code(),
                    words: header.payload_words(),
                })
            );
        }
        // The same header over a payload of exactly its length.
        let exact = vec![0xA5; usize::from(header.payload_words()) * 4];
        let _ = Command::parse(header, &exact);
    }

    /// Every reserved or unused byte of a command written is zero, and so is
    /// every reserved bit of an Invitation's Authentication Methods byte.
    /// Spec: m2-124/5.3#5, m2-124/5.4#1
    #[test]
    fn every_reserved_field_and_unused_byte_of_a_written_command_is_zero(command in command()) {
        let mut bytes = Vec::new();
        command.write(&mut bytes);
        for range in reserved_bytes(&command) {
            prop_assert!(
                bytes[range.clone()].iter().all(|&b| b == 0),
                "{command:?} wrote {:?} in {range:?}",
                &bytes[range.clone()]
            );
        }
        if matches!(command, Command::Invitation(_)) {
            prop_assert_eq!(bytes[3] & !0b11, 0, "reserved method bits");
        }
    }

    /// A command written is the command read back, from its own datagram.
    #[test]
    fn a_command_written_is_the_command_parsed(commands in proptest::collection::vec(command(), 1..6)) {
        let mut builder = DatagramBuilder::new();
        for command in &commands {
            builder.push(command);
        }
        let bytes = builder.finish();
        let read: Vec<_> = parse(&bytes)
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        prop_assert_eq!(read, commands);
    }
}

/// A datagram a peer might send: arbitrary bytes after the signature, or
/// commands that were written whole.
fn datagram() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..200).prop_map(|bytes| signed(&bytes)),
        proptest::collection::vec(command(), 1..4).prop_map(|commands| {
            let mut builder = DatagramBuilder::new();
            for command in &commands {
                builder.push(command);
            }
            builder.finish()
        }),
    ]
}

fn peer(index: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, index + 1], 50_000))
}

/// What an endpoint sent is a datagram a peer reads whole.
fn assert_readable(endpoint: &mut impl NetworkEndpoint) -> Result<(), TestCaseError> {
    while let Some(transmit) = endpoint.poll_transmit() {
        let commands = parse(&transmit.payload);
        prop_assert!(commands.is_ok(), "sent without the signature");
        for command in commands.unwrap() {
            prop_assert!(
                command.is_ok(),
                "sent a command a peer refuses: {command:?}"
            );
        }
    }
    while endpoint.poll_event().is_some() {}
    Ok(())
}

proptest! {
    /// A Host fed any datagrams from a few peers, with the clock moving between
    /// them, never panics and answers only in datagrams a peer can read.
    #[test]
    fn any_datagrams_into_a_host_never_panic_and_the_answers_are_readable(
        steps in proptest::collection::vec((0u8..3, datagram(), 0u64..3_000), 1..40),
        admission in 0u8..4,
    ) {
        let identity = identity("Studio", "HOST-1");
        let mut config = HostConfig::new(identity);
        config.admission = match admission {
            0 => Admission::Open,
            1 => Admission::Confirm,
            2 => Admission::SharedSecret("1234".to_owned()),
            _ => Admission::Users(vec![(Username::new("Rosa").unwrap(), "pw".to_owned())]),
        };
        let mut host = Host::new(config);
        let mut now = Instant::now();
        for (from, bytes, advance_ms) in steps {
            host.on_message(peer(from), &bytes, now);
            now += Duration::from_millis(advance_ms);
            host.on_timeout(now);
            assert_readable(&mut host)?;
        }
    }

    /// A Client fed any datagrams from its Host never panics and answers only
    /// in datagrams a peer can read.
    #[test]
    fn any_datagrams_into_a_client_never_panic_and_the_answers_are_readable(
        steps in proptest::collection::vec((datagram(), 0u64..3_000), 1..40),
        secret in any::<bool>(),
    ) {
        let identity = identity("Laptop", "CLIENT-1");
        let credentials = if secret {
            Credentials::none().secret("1234")
        } else {
            Credentials::none()
        };
        let mut now = Instant::now();
        let mut client = Client::new(identity, credentials, SessionConfig::default(), peer(0), now);
        client.connect(now);
        for (bytes, advance_ms) in steps {
            client.on_message(peer(0), &bytes, now);
            now += Duration::from_millis(advance_ms);
            client.on_timeout(now);
            assert_readable(&mut client)?;
        }
    }
}
