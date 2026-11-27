//! The diagnostic loopback endpoints: the service always lists two, cross-wired, so what
//! is sent to one arrives at the other.

use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use tutti_platform::Instant;
use tutti_ump::Ump;
use tutti_winmidi2_ffi::{
    Clock, Connection, Endpoint, Error, Loopback, Session, endpoints, ensure_service_available,
};

/// Only a hang guard: it bounds a wait for a message that never comes, and is not a speed.
const HANG_GUARD: Duration = Duration::from_secs(60);

fn loopback(which: Loopback) -> Endpoint {
    ensure_service_available().expect("the MIDI service is available");
    endpoints()
        .expect("the service lists its endpoints")
        .into_iter()
        .find(|e| e.loopback == Some(which))
        .unwrap_or_else(|| panic!("the service lists diagnostic loopback {which:?}"))
}

/// A session with a connection that sends to `to` and one that receives what arrives at
/// `from`'s partner: what is sent on `to` arrives on the other loopback.
struct Pair {
    session: Session,
    sender: Connection,
    received: Receiver<(Option<Instant>, Ump)>,
    // The receiving connection stays open for the test.
    _receiver: Connection,
}

fn pair(send_to: Loopback) -> Pair {
    let (receive_on, name) = match send_to {
        Loopback::A => (Loopback::B, "tutti-winmidi2-ffi A to B"),
        Loopback::B => (Loopback::A, "tutti-winmidi2-ffi B to A"),
    };
    let (send_to, receive_on) = (loopback(send_to), loopback(receive_on));
    let session = Session::create(name).expect("a session");
    let (tx, received) = channel();
    let receiver = session
        .connect(&receive_on.id, move |at, ump| {
            // The test may have ended: a send to a dropped receiver is the end.
            let _ = tx.send((at, ump));
        })
        .expect("a connection to the receiving loopback endpoint");
    let sender = session
        .connect(&send_to.id, |_, _| {})
        .expect("a connection to the sending loopback endpoint");
    Pair {
        session,
        sender,
        received,
        _receiver: receiver,
    }
}

/// One message of each length a UMP has, then a run that differs in every word.
fn messages() -> Vec<Ump> {
    let mut messages = vec![
        // A System Real Time Timing Clock: 32 bits.
        Ump([0x10F8_0000, 0, 0, 0]),
        // A MIDI 2.0 Note On: 64 bits.
        Ump([0x4090_3C00, 0x8000_0000, 0, 0]),
        // A Flex Data Set Tempo: 128 bits.
        Ump([0xD010_0000, 0x02FA_F080, 0, 0]),
    ];
    messages.extend((0..32).map(|i| Ump([0x4091_0000 | (i << 8), 0xC000_0000 | i, 0, 0])));
    messages
}

fn receive_all(pair: &Pair, count: usize) -> Vec<(Option<Instant>, Ump)> {
    (0..count)
        .map(|n| {
            pair.received
                .recv_timeout(HANG_GUARD)
                .unwrap_or_else(|e| panic!("message {n} of {count} never arrived: {e}"))
        })
        .collect()
}

fn every_word_arrives_unchanged_and_in_order(send_to: Loopback) {
    let pair = pair(send_to);
    let sent = messages();
    for ump in &sent {
        pair.sender
            .send(None, ump)
            .expect("the service takes the message");
    }
    let arrived: Vec<Ump> = receive_all(&pair, sent.len())
        .into_iter()
        .map(|(_, ump)| ump)
        .collect();
    assert_eq!(arrived, sent);
    pair.session.close().expect("the session closes");
}

fn a_timestamp_is_no_earlier_than_the_send(send_to: Loopback) {
    let pair = pair(send_to);
    let sent = messages();
    let stamps: Vec<Instant> = sent
        .iter()
        .map(|ump| {
            let at = Instant::now();
            pair.sender
                .send(Some(at), ump)
                .expect("the service takes the message");
            at
        })
        .collect();
    let arrived = receive_all(&pair, sent.len());
    for ((ump, sent_at), (at, got)) in sent.iter().zip(&stamps).zip(&arrived) {
        assert_eq!(got, ump);
        let at = at.expect("a timestamp of the service's clock is an Instant");
        assert!(at >= *sent_at, "{ump:?} arrived stamped before it was sent");
    }
}

/// **Every word sent on loopback A arrives at B unchanged and in order**, a message of each
/// length and a run of messages that differ in every word.
#[test]
fn every_word_sent_to_a_arrives_at_b_unchanged_and_in_order() {
    every_word_arrives_unchanged_and_in_order(Loopback::A);
}

/// **Every word sent on loopback B arrives at A unchanged and in order.**
#[test]
fn every_word_sent_to_b_arrives_at_a_unchanged_and_in_order() {
    every_word_arrives_unchanged_and_in_order(Loopback::B);
}

/// **A message sent at an `Instant` arrives stamped no earlier than it**, from A to B.
#[test]
fn a_message_sent_to_a_arrives_stamped_no_earlier_than_its_send() {
    a_timestamp_is_no_earlier_than_the_send(Loopback::A);
}

/// **A message sent at an `Instant` arrives stamped no earlier than it**, from B to A.
#[test]
fn a_message_sent_to_b_arrives_stamped_no_earlier_than_its_send() {
    a_timestamp_is_no_earlier_than_the_send(Loopback::B);
}

/// **A closed connection refuses a send**, and closing it again does nothing.
#[test]
fn a_closed_connection_refuses_a_send() {
    let pair = pair(Loopback::A);
    let ump = Ump([0x4090_3C00, 0x8000_0000, 0, 0]);
    pair.sender
        .send(None, &ump)
        .expect("an open connection sends");
    pair.sender.close();
    pair.sender.close();
    assert_eq!(pair.sender.send(None, &ump), Err(Error::Closed));
}

/// **The service lists both loopback endpoints once, each with a name.**
#[test]
fn the_service_lists_each_loopback_endpoint_once() {
    ensure_service_available().expect("the MIDI service is available");
    let listed = endpoints().expect("the service lists its endpoints");
    for which in [Loopback::A, Loopback::B] {
        let found: Vec<&Endpoint> = listed
            .iter()
            .filter(|e| e.loopback == Some(which))
            .collect();
        assert_eq!(found.len(), 1, "{which:?} in {listed:?}");
        assert!(!found[0].name.is_empty() && !found[0].id.is_empty());
    }
}

/// **The service's clock is the clock `Instant` reads**: a reading lies between two reads
/// of `Instant::now`.
#[test]
fn the_services_clock_is_the_instant_clock() {
    let clock = Clock::new().expect("the API's clock");
    let before = Instant::now();
    let now = clock.now().expect("a reading");
    let after = Instant::now();
    assert!(
        before <= now && now <= after,
        "{before:?} {now:?} {after:?}"
    );
}
