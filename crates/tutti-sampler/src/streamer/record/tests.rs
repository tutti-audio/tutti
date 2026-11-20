//! The step table: what each command does to the table, driven with no thread,
//! no streamer, no file and no queue.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use tutti_buffers::ChannelLayout;
use tutti_platform::Asset;
use tutti_units::SampleRate;

use super::*;
use crate::streamer::cache::LruCache;
use crate::streamer::prefetch::RegionBuffer;
use crate::streamer::prefetch::{Ring, SharedReader};
use tutti_rt::Publisher;

/// A record on a fresh ring of `capacity` frames of `channels`, with the ring
/// and the reader's half: what a stream's open builds, minus the file.
pub(in crate::streamer) fn record(
    id: StreamId,
    path: PathBuf,
    capacity: usize,
    channels: impl Into<ChannelLayout>,
) -> (Record<f32>, SharedReader<f32>, tutti_rt::PosReader) {
    let (rec, ring, pos, _status) = record_with_status(id, path, capacity, channels);
    (rec, ring, pos)
}

/// [`record`], and the stream's status cell the handle and the voice share.
fn record_with_status(
    id: StreamId,
    path: PathBuf,
    capacity: usize,
    channels: impl Into<ChannelLayout>,
) -> (
    Record<f32>,
    SharedReader<f32>,
    tutti_rt::PosReader,
    Arc<StatusCell>,
) {
    let (record, ring, pos, status, _, _) = record_with_cells(id, path, capacity, channels);
    (record, ring, pos, status)
}

/// [`record_with_status`], and the cells the control side asks its loop and
/// direction through.
fn record_with_cells(
    id: StreamId,
    path: PathBuf,
    capacity: usize,
    channels: impl Into<ChannelLayout>,
) -> (
    Record<f32>,
    SharedReader<f32>,
    tutti_rt::PosReader,
    Arc<StatusCell>,
    Publisher<Option<Loop>>,
    Publisher<Direction>,
) {
    let (out, ring, pos) = RegionBuffer::with_capacity(id, path.clone(), capacity, channels);
    let cache = Arc::new(LruCache::<f32>::new(4, 1 << 20));
    let pin = cache.pin(Asset::from(path).identity().as_ref());
    let status = Arc::new(StatusCell::new());
    let looping = Publisher::new(None);
    let direction = Publisher::new(Direction::Forward);
    let record = Record::new(
        out,
        Arc::new(Cells::new()),
        SampleRate::SR_48K,
        Samples::new(1_000),
        pin,
        Arc::clone(&status),
        looping.reader(),
        direction.reader(),
    );
    (record, ring, pos, status, looping, direction)
}

fn open(streams: &mut Streams<f32>, id: u64) -> (Weak<Ring<f32>>, Arc<StatusCell>) {
    let (rec, ring, _pos, status) =
        record_with_status(StreamId::of(id), PathBuf::from("t.wav"), 1024, 1u16);
    let weak = Arc::downgrade(&ring);
    let _ = step(streams, Cmd::Open(Box::new(rec)));
    (weak, status)
}

fn live(streams: &mut Streams<f32>, id: u64) -> (Weak<Ring<f32>>, Arc<StatusCell>) {
    let opened = open(streams, id);
    let _ = step(streams, Cmd::Opened(StreamId::of(id)));
    opened
}

fn ask(start: u64, end: u64) -> Option<Loop> {
    Some(crate::testing::loop_frames(start as usize, end as usize, 0))
}

/// An open is a record in `Opening`, which a refill leaves alone; its
/// reader's delivery makes it `Live`, once.
#[test]
fn an_open_is_opening_until_its_reader_is_delivered() {
    let mut streams = Streams::new();
    let (_ring, _status) = open(&mut streams, 1);
    let id = StreamId::of(1);
    assert_eq!(streams.len(), 1);
    assert_eq!(streams.get(id).unwrap().phase(), Phase::Opening);
    assert!(!streams.get(id).unwrap().is_live());

    assert_eq!(step(&mut streams, Cmd::Opened(id)), &[Effect::Live(id)]);
    assert_eq!(streams.get(id).unwrap().phase(), Phase::Live);
    assert!(
        step(&mut streams, Cmd::Opened(id)).is_empty(),
        "delivered once"
    );
}

/// A close frees the stream's region in the step that handles it: the ring
/// (its writer, decoder and resident wave with it) is dead when the step
/// returns, the stream's status says it is closed (what a fork of its voice
/// asks), and the table does not hold it.
#[test]
fn a_close_frees_the_region_in_that_step() {
    let mut streams = Streams::new();
    let (ring, status) = live(&mut streams, 1);
    let id = StreamId::of(1);
    assert!(ring.upgrade().is_some(), "setup: the writer holds the ring");
    assert!(!status.closed(), "setup: the stream runs");

    assert_eq!(step(&mut streams, Cmd::Close(id)), &[Effect::Freed(id)]);
    assert!(
        ring.upgrade().is_none(),
        "the closed stream's ring outlived its step"
    );
    assert!(streams.is_empty());
    assert!(status.closed(), "a fork of it plays nothing");

    // A stream still `Opening` closes the same way (an abandoned open).
    let (ring, _status) = open(&mut streams, 2);
    let _ = step(&mut streams, Cmd::Close(StreamId::of(2)));
    assert!(ring.upgrade().is_none());
}

/// A command for an id the table does not hold changes nothing and asks for
/// nothing, and never reaches another stream. Ids are minted once, so the
/// stream that took a closed one's place has a new id, and what was sent to
/// the old one finds nobody.
#[test]
fn a_command_for_an_unknown_stream_reaches_no_other() {
    let mut streams = Streams::new();
    let (ring_a, _) = live(&mut streams, 1);
    let _ = step(&mut streams, Cmd::Close(StreamId::of(1)));
    assert!(ring_a.upgrade().is_none());
    let (ring_b, _) = live(&mut streams, 2);

    assert!(
        step(&mut streams, Cmd::Close(StreamId::of(1))).is_empty(),
        "closing twice frees nothing more"
    );
    assert!(step(&mut streams, Cmd::Opened(StreamId::of(1))).is_empty());
    assert_eq!(streams.len(), 1);
    assert!(ring_b.upgrade().is_some(), "the new stream is untouched");
}

/// Streams are independent: closing one leaves the others held, found by
/// their own ids after the `swap_remove` has moved one.
#[test]
fn streams_are_independent() {
    let mut streams = Streams::new();
    let rings: Vec<_> = (1..=3).map(|i| live(&mut streams, i).0).collect();
    let _ = step(&mut streams, Cmd::Close(StreamId::of(1)));
    assert!(rings[0].upgrade().is_none());
    assert!(rings[1].upgrade().is_some() && rings[2].upgrade().is_some());
    assert!(streams.get(StreamId::of(3)).is_some() && streams.get(StreamId::of(2)).is_some());
}

/// An open whose id the table already holds is dropped, and freed, rather than
/// shadowing the stream held. Ids are minted once, so this cannot happen
/// through the control side; the step still refuses it.
#[test]
fn a_duplicate_open_does_not_shadow() {
    let mut streams = Streams::new();
    let (first, _) = open(&mut streams, 1);
    let (second, _) = open(&mut streams, 1);
    assert_eq!(streams.len(), 1);
    assert!(first.upgrade().is_some());
    assert!(second.upgrade().is_none(), "the duplicate is freed");
}

/// **A loop or direction the control side publishes is taken once.** A stream
/// starts asked for no loop and forwards; a published value that differs is
/// taken on the next look, and a look at the same value (published again or
/// not) takes nothing; a clear and a turn back are changes too.
#[test]
fn a_published_request_is_taken_once_when_it_changes() {
    let (mut rec, _ring, _pos, _status, looping, direction) =
        record_with_cells(StreamId::of(1), PathBuf::from("t.wav"), 1024, 1u16);
    assert_eq!(rec.loop_asked(), None, "no loop asked at the start");
    assert_eq!(rec.direction_asked(), None, "forwards at the start");

    looping.publish(ask(0, 10));
    assert_eq!(rec.loop_asked(), Some(ask(0, 10)));
    assert_eq!(rec.loop_asked(), None, "taken once");
    looping.publish(ask(0, 10));
    assert_eq!(
        rec.loop_asked(),
        None,
        "the same loop again changes nothing"
    );
    looping.publish(ask(1, 2));
    looping.publish(ask(5, 9));
    assert_eq!(rec.loop_asked(), Some(ask(5, 9)), "the latest of two");
    looping.publish(None);
    assert_eq!(rec.loop_asked(), Some(None), "a clear is a request");

    direction.publish(Direction::Reverse);
    assert_eq!(rec.direction_asked(), Some(Direction::Reverse));
    assert_eq!(rec.direction_asked(), None, "taken once");
    direction.publish(Direction::Forward);
    assert_eq!(rec.direction_asked(), Some(Direction::Forward));
}

/// **A request published before the stream is live is taken when it goes
/// live**: a cycle leaves an `Opening` stream's loop and direction alone, and
/// takes them, once, in the cycle after its reader is delivered.
#[test]
fn a_request_published_before_the_stream_is_live_is_taken_once_it_is() {
    use crate::streamer::config::BufferConfig;
    use crate::streamer::handlers::{Local, follow_steering};
    let config = BufferConfig::default();
    let mut local = Local::<f32>::new(config.chunk_size);
    let id = StreamId::of(1);
    let (rec, _ring, _pos, _status, looping, direction) =
        record_with_cells(id, PathBuf::from("t.wav"), 1024, 1u16);
    let _ = step(&mut local.streams, Cmd::Open(Box::new(rec)));
    looping.publish(ask(0, 10));
    direction.publish(Direction::Reverse);

    follow_steering(&config, &mut local);
    let held = local.streams.get(id).unwrap();
    assert_eq!(
        (held.asked_loop, held.asked_direction),
        (None, Direction::Forward),
        "an opening stream takes nothing"
    );

    let _ = step(&mut local.streams, Cmd::Opened(id));
    follow_steering(&config, &mut local);
    let held = local.streams.get(id).unwrap();
    assert_eq!(
        (held.asked_loop, held.asked_direction),
        (ask(0, 10), Direction::Reverse),
        "taken once it is live"
    );
}

/// A rate event re-derives each stream's own ratio from its own file's rate:
/// two streams at different file rates, one session rate, two ratios; and the
/// ratio is the cell the stream's voice reads.
#[test]
fn a_rate_event_re_derives_each_streams_own_ratio() {
    use tutti_units::SrcRatio;
    let mut streams = Streams::<f32>::new();
    let cells: Vec<_> = [44_100.0, 48_000.0]
        .into_iter()
        .zip(1..)
        .map(|(file_rate, id)| {
            let (out, _ring, _pos) =
                RegionBuffer::with_capacity(StreamId::of(id), PathBuf::new(), 1024, 1u16);
            let cache = Arc::new(LruCache::<f32>::new(4, 1 << 20));
            let cells = Arc::new(Cells::new());
            let record = Record::new(
                out,
                Arc::clone(&cells),
                SampleRate::of(file_rate),
                Samples::new(1_000),
                cache.pin(None),
                Arc::new(StatusCell::new()),
                Publisher::new(None).reader(),
                Publisher::new(Direction::Forward).reader(),
            );
            let _ = step(&mut streams, Cmd::Open(Box::new(record)));
            cells
        })
        .collect();
    streams.rerate(SampleRate::SR_48K);
    for (cells, file_rate) in cells.iter().zip([44_100.0, 48_000.0]) {
        assert_eq!(
            cells.src_ratio(),
            SrcRatio::for_rates(SampleRate::of(file_rate), SampleRate::SR_48K)
        );
    }
    assert_eq!(cells[1].src_ratio(), SrcRatio::UNITY);
    assert!(cells[0].src_ratio() != SrcRatio::UNITY);
}
