//! The diagnostic loopback endpoints: the service always lists two, cross-wired, so what
//! is sent to one arrives at the other.

use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use crate::deadline::{Deadline, HANG_GUARD};
use tutti_platform::Instant;
use tutti_ump::Ump;
use tutti_winmidi2_ffi::{
    Connection, Endpoint, Error, Loopback, Session, endpoints, ensure_service_available,
};

fn loopback(which: Loopback) -> Endpoint {
    ensure_service_available().expect("the MIDI service is available");
    endpoints()
        .expect("the service lists its endpoints")
        .into_iter()
        .find(|e| e.loopback == Some(which))
        .unwrap_or_else(|| panic!("the service lists diagnostic loopback {which:?}"))
}

/// One message the receiving connection was handed: its timestamp, and the time the handler
/// ran.
struct Received {
    stamp: Option<Instant>,
    ump: Ump,
    arrived: Instant,
}

/// A session with a connection that sends to `to` and one that receives what arrives at
/// `from`'s partner: what is sent on `to` arrives on the other loopback.
struct Pair {
    session: Session,
    sender: Connection,
    received: Receiver<Received>,
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
        .connect(&receive_on.id, move |received| {
            // The test may have ended: a send to a dropped receiver is the end. A message
            // that cannot be read is no message, and the test then waits in vain for it.
            if let Ok((stamp, ump)) = received {
                let _ = tx.send(Received {
                    stamp,
                    ump,
                    arrived: Instant::now(),
                });
            }
        })
        .expect("a connection to the receiving loopback endpoint");
    let sender = session
        .connect_send_only(&send_to.id)
        .expect("a send-only connection to the sending loopback endpoint");
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

fn receive_all(pair: &Pair, count: usize) -> Vec<Received> {
    let deadline = Deadline::start();
    (0..count)
        .map(|n| {
            deadline
                .recv(&pair.received)
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
        .map(|received| received.ump)
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
    for ((ump, sent_at), got) in sent.iter().zip(&stamps).zip(&arrived) {
        assert_eq!(&got.ump, ump);
        let at = got
            .stamp
            .expect("a timestamp of the service's clock is an Instant");
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

/// How far ahead a scheduled message is sent: only a point the service must hold the message
/// until, never a speed asserted.
const AHEAD: Duration = Duration::from_millis(250);

/// How early the service's scheduler may release a message: it sends once the time is within
/// its send window, `MIDI_SCHEDULER_LOCK_AND_SEND_FUNCTION_LATENCY_TICKS` (100) plus
/// `MIDI_SCHEDULER_ENQUEUE_OVERHEAD_LATENCY_TICKS` (10) ticks of its 10 MHz clock, and its
/// output latency compensation for the endpoint on top of that, which a loopback does not
/// declare. 110 ticks are 11 microseconds.
const SEND_WINDOW: Duration = Duration::from_micros(11);

/// **A message sent at a future `Instant` is held by the service until then**: its handler
/// runs no earlier than that instant, less the scheduler's send window. A message the service
/// delivered at once would run its handler `AHEAD` early. The stamp is not pinned: the service
/// echoes the timestamp it was sent with.
#[test]
fn a_message_sent_ahead_does_not_arrive_before_its_time() {
    let pair = pair(Loopback::A);
    let ump = Ump([0x4090_3C00, 0x8000_0000, 0, 0]);
    let at = Instant::now() + AHEAD;
    pair.sender
        .send(Some(at), &ump)
        .expect("the service takes the message");
    let [got] = receive_all(&pair, 1).try_into().ok().expect("one message");
    assert_eq!(got.ump, ump);
    assert!(
        got.arrived + SEND_WINDOW >= at,
        "the message arrived before its time"
    );
}

/// **A message sent at an `Instant` that has passed is sent at once**: the service takes it
/// and delivers it, stamped with the time it was sent at and no later than it arrived.
#[test]
fn a_message_sent_at_a_past_instant_arrives() {
    let pair = pair(Loopback::A);
    let ump = Ump([0x4090_3C00, 0x8000_0000, 0, 0]);
    let past = Instant::now()
        .checked_sub(AHEAD)
        .expect("the clock has run longer than the delay");
    pair.sender
        .send(Some(past), &ump)
        .expect("the service takes the message");
    let [got] = receive_all(&pair, 1).try_into().ok().expect("one message");
    assert_eq!(got.ump, ump);
    let stamp = got
        .stamp
        .expect("a timestamp of the service's clock is an Instant");
    assert!(stamp >= past && stamp <= got.arrived, "{past:?} {stamp:?}");
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

/// **A handler that panics on its first message does not stop the connection**: the panic is
/// caught, and the second message sent on the same connection still reaches the handler.
#[test]
fn a_panicking_handler_does_not_stop_later_messages() {
    let (send_to, receive_on) = (loopback(Loopback::A), loopback(Loopback::B));
    let session = Session::create("tutti-winmidi2-ffi panic guard").expect("a session");
    let (tx, received) = channel();
    let mut first = true;
    let _receiver = session
        .connect(&receive_on.id, move |received| {
            assert!(
                !std::mem::take(&mut first),
                "the handler's first message panics"
            );
            if let Ok((_, ump)) = received {
                let _ = tx.send(ump);
            }
        })
        .expect("a connection to the receiving loopback endpoint");
    let sender = session
        .connect(&send_to.id, |_| {})
        .expect("a connection to the sending loopback endpoint");
    let (one, two) = (
        Ump([0x4090_3C00, 0x8000_0000, 0, 0]),
        Ump([0x4090_3E00, 0x8000_0001, 0, 0]),
    );
    sender
        .send(None, &one)
        .expect("the service takes the message");
    sender
        .send(None, &two)
        .expect("the service takes the message");
    assert_eq!(received.recv_timeout(HANG_GUARD), Ok(two));
}
