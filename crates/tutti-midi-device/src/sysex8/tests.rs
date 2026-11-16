use tutti_ump::MidiGroup;

use super::*;
use crate::FunctionBlock;
use tutti_ump::{Direction, FirstGroup, FunctionBlockId, GroupSpan};

fn ids(streams: &mut Sysex8Streams, count: usize) -> Vec<u8> {
    (0..count)
        .map(|_| streams.open().unwrap().stream_id())
        .collect()
}

// §7.8: "A device which supports only one stream shall use 0 as the Stream ID."
// A count of 1 is "does not support multiple, simultaneous" messages (§7.1.8),
// and the Id is 0 again once the message is released.
/// Spec: m2-104/7.8#5
#[test]
fn a_receiver_with_one_stream_is_sent_stream_id_0() {
    let mut streams = Sysex8Streams::new(Sysex8Support::declared(1));
    let slot = streams.open().unwrap();
    assert_eq!(slot.stream_id(), 0);
    assert_eq!(streams.open(), Err(Sysex8Refused::Busy));
    streams.release(slot);
    assert_eq!(streams.open().unwrap().stream_id(), 0);
}

// §7.8: "the Sender shall not send more than one simultaneous stream" when the
// Receiver's support is not discovered: a second message waits for the first.
/// Spec: m2-104/7.8#6
#[test]
fn without_discovery_one_stream_is_open_at_a_time() {
    let mut streams = Sysex8Streams::new(Sysex8Support::undiscovered());
    let first = streams.open().unwrap();
    assert_eq!(first.stream_id(), 0);
    assert_eq!(streams.open(), Err(Sysex8Refused::Busy));
    streams.release(first);
    assert!(streams.open().is_ok());
}

// §7.8: "only Stream IDs from 0 to (N-1) shall be used", at the ends of the
// §7.1.8 range: 2 streams are 0 and 1, 255 are 0 to 254.
/// Spec: m2-104/7.8#7
#[test]
fn n_streams_use_stream_ids_0_to_n_minus_1() {
    let mut two = Sysex8Streams::new(Sysex8Support::declared(2));
    assert_eq!(ids(&mut two, 2), [0, 1]);
    assert_eq!(two.open(), Err(Sysex8Refused::Busy));

    let mut most = Sysex8Streams::new(Sysex8Support::declared(255));
    assert_eq!(ids(&mut most, 255), (0..=254).collect::<Vec<u8>>());
    assert_eq!(most.open(), Err(Sysex8Refused::Busy));
}

// A released Id is the lowest free one for the next message, so Ids stay in
// 0..N under reuse.
/// Spec: m2-104/7.8#7
#[test]
fn a_released_stream_id_is_reused_below_the_count() {
    let mut streams = Sysex8Streams::new(Sysex8Support::declared(3));
    let (a, b, c) = (
        streams.open().unwrap(),
        streams.open().unwrap(),
        streams.open().unwrap(),
    );
    streams.release(b);
    let again = streams.open().unwrap();
    assert_eq!(again.stream_id(), 1);
    assert_eq!(streams.open(), Err(Sysex8Refused::Busy));
    drop((a, c));
}

// §7.1.8: a count of 0 is "Receiver does not support any System Exclusive 8
// messages": no Id is given.
/// Spec: m2-104/7.8#5
#[test]
fn a_receiver_with_no_sysex8_is_given_no_stream_id() {
    let mut streams = Sysex8Streams::new(Sysex8Support::declared(0));
    assert_eq!(streams.open(), Err(Sysex8Refused::Unsupported));
}

// §7.8: a merger "shall first perform a Function Block Discovery message to
// determine how many simultaneous Stream IDs are supported". Before the Function
// Block Info arrives a merger holds one stream, so it cannot merge two; the
// block's own count (here 2) is what lets it.
/// Spec: m2-104/7.8#8
#[test]
fn a_merger_sends_merged_streams_only_after_the_receiver_declares_them() {
    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        Direction::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 2)
    .build()
    .unwrap();
    let mut merger = Sysex8Streams::new(Sysex8Support::undiscovered());
    let from_a = merger.open().unwrap();
    assert_eq!(
        merger.open(),
        Err(Sysex8Refused::Busy),
        "not yet discovered"
    );

    merger.rediscover(Sysex8Support::of_block(&block));
    let from_b = merger.open().unwrap();
    assert_eq!((from_a.stream_id(), from_b.stream_id()), (0, 1));
    let message = from_b.message(MidiGroup::of(3), &[1, 2]);
    assert_eq!((message.group, message.stream_id), (MidiGroup::of(3), 1));
}
