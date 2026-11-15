//! A change of the root's width opens again what reads the root: a tap on it
//! and the master meter arrive as `SessionEvent::TapReopened` with the new
//! topology, the old reader reads closed, and a reader of a port whose width
//! did not change is left alone.
//!
//! Every figure is analytic: the source is the constant frame (0.25, 0.75), so
//! the stereo root carries 0.25 and 0.75 and a mono root keeps channel 0.

use tutti_buffers::{InterleavedMut, Pull, Source};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{OutPort, Reopened, Root, Session, SessionEvent};
use tutti_types::{ChannelLayout, ChannelTopology, MaxBlock, SampleRate, Samples};

const BLOCK: usize = 64;

/// A stereo session whose root is fed (0.25, 0.75), the plan installed.
fn session() -> Session {
    let mut s = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples(BLOCK)))
        .build()
        .expect("builds");
    let (node, _) = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.75])));
    for c in 0..2 {
        s.graph()
            .set_sources_hard(Root::output(c), [OutPort::new(node, c)])
            .unwrap();
    }
    let _ = s.update();
    s
}

/// One update, then one block rendered; the update's events.
fn block(s: &mut Session) -> Vec<SessionEvent> {
    let events = s.update();
    let mut out = vec![0.0f32; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    events
}

fn mono() -> ChannelTopology {
    ChannelTopology::assumed(ChannelLayout::MONO)
}

/// The readers `events` reopened.
fn reopened(events: Vec<SessionEvent>) -> Vec<(Reopened, ChannelTopology)> {
    events
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::TapReopened { reader, topology } => Some((reader, topology)),
            _ => None,
        })
        .collect()
}

/// Everything `tap` reads until it starves or ends, interleaved, and whether
/// it ended.
fn drain(tap: &mut tutti_graph::TapReader) -> (Vec<f32>, bool) {
    let width = tap.width();
    let mut all = Vec::new();
    loop {
        let mut chunk = vec![0.0f32; width * 16];
        let layout = tap.layout();
        match tap.pull(InterleavedMut::new(&mut chunk, layout).unwrap()) {
            Pull::Frames(n) => all.extend_from_slice(&chunk[..n.get() * width]),
            Pull::Ended => return (all, true),
            _ => return (all, false),
        }
    }
}

/// **A root change mid-stream reopens its tap once, with the new topology;
/// the old reader drains and reads closed; the new one reads the new width.**
/// The old reader holds the three blocks written before the commit, none lost;
/// the new reader holds the blocks from the commit's block on, one channel of
/// 0.25 (the mono root keeps channel 0).
#[test]
fn a_root_width_change_reopens_the_root_tap_and_closes_the_old_reader() {
    let mut s = session();
    let mut old = s
        .graph()
        .taps()
        .open_output(Samples(4096))
        .expect("the outputs exist");
    assert_eq!(old.width(), 2);
    for _ in 0..3 {
        assert!(reopened(block(&mut s)).is_empty());
    }

    s.declare_root_topology(Some(mono()))
        .expect("a mono root")
        .hard();
    let mut events = reopened(s.update());
    assert_eq!(events.len(), 1, "one reopen for the one tap");
    let (Reopened::Tap(mut new), topology) = events.remove(0) else {
        panic!("a tap")
    };
    assert_eq!(topology, mono(), "carries the new topology");
    assert_eq!(new.width(), 1);
    assert_eq!(new.id(), old.id(), "the new reader takes the old one's id");

    let mut out = vec![0.0f32; BLOCK * 2];
    for _ in 0..2 {
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        let _ = s.update();
    }

    let (old_frames, ended) = drain(&mut old);
    assert!(ended, "the old reader reads closed once drained");
    assert_eq!(
        old_frames.len(),
        3 * BLOCK * 2,
        "the frames before the commit"
    );
    assert!(
        old_frames
            .as_chunks::<2>()
            .0
            .iter()
            .all(|f| f[0] == 0.25 && f[1] == 0.75),
        "the old reader holds the stereo root"
    );
    assert_eq!(
        old.take_lost(),
        Samples(0),
        "nothing at the handoff is lost"
    );

    let (new_frames, new_ended) = drain(&mut new);
    assert!(!new_ended);
    assert_eq!(
        new_frames.len(),
        2 * BLOCK,
        "from the first block of the new plan"
    );
    assert!(
        new_frames.iter().all(|&x| x == 0.25),
        "the mono root keeps channel 0"
    );
    assert_eq!(new.take_lost(), Samples(0));
}

/// **A tap on a port whose width did not change is not reopened**: a
/// declaration of the stereo the root already is changes the declaration and
/// not the width, so no event arrives and the reader keeps reading.
#[test]
fn a_root_declaration_of_the_same_width_leaves_the_tap_alone() {
    let mut s = session();
    let mut tap = s
        .graph()
        .taps()
        .open_output(Samples(4096))
        .expect("the outputs exist");
    let _ = block(&mut s);
    s.declare_root_topology(Some(ChannelTopology::assumed(ChannelLayout::STEREO)))
        .expect("a stereo root")
        .hard();
    for _ in 0..3 {
        assert!(reopened(block(&mut s)).is_empty());
    }
    let (frames, ended) = drain(&mut tap);
    assert!(!ended, "the reader is still open");
    assert_eq!(frames.len(), 4 * BLOCK * 2);
}

/// **A meter reopens with the new width**: the master meter a host took
/// arrives with the new topology, measuring one channel at 0.25 (the peak and,
/// the source being constant, the rms); the old handle reads closed.
#[test]
fn the_taken_master_meter_reopens_with_the_new_width() {
    let mut s = session();
    let old = s.take_master_meter().expect("held");
    assert_eq!(old.channels(), 2);
    let _ = block(&mut s);
    assert!(!old.is_closed());

    s.declare_root_topology(Some(mono()))
        .expect("a mono root")
        .hard();
    let mut events = reopened(block(&mut s));
    assert_eq!(events.len(), 1, "the meter, and no tap, was opened");
    let (Reopened::Meter(mut new), topology) = events.remove(0) else {
        panic!("a meter")
    };
    assert_eq!(topology, mono());
    assert!(new.is_enabled());
    let _ = block(&mut s);
    let _ = s.update();
    assert!(old.is_closed(), "the old handle reads closed");
    assert_eq!(new.channels(), 1);
    let levels = new.read();
    assert_eq!(levels.len(), 1);
    assert_eq!(levels[0].peak.get(), 0.25);
    assert_eq!(levels[0].rms.get(), 0.25);
}

/// **The master meter a host did not take follows the root in place**: no
/// event, and the graph's own reader is the new one.
#[test]
fn the_held_master_meter_is_replaced_in_place() {
    let mut s = session();
    s.declare_root_topology(Some(mono()))
        .expect("a mono root")
        .hard();
    assert!(reopened(block(&mut s)).is_empty());
    let _ = block(&mut s);
    let meter = s.master_meter().expect("held");
    assert_eq!(meter.channels(), 1);
    assert_eq!(meter.read()[0].peak.get(), 0.25);
}

/// **Two changes before one update reopen once, at the last width**: the
/// reader of the first change was never sent, and is not reported.
#[test]
fn two_changes_before_an_update_report_the_last_reader_only() {
    let mut s = session();
    let mut old = s.graph().taps().open_output(Samples(4096)).expect("opens");
    let _ = block(&mut s);
    s.declare_root_topology(Some(mono())).expect("mono").hard();
    s.declare_root_topology(Some(ChannelTopology::assumed(ChannelLayout::from_count(4))))
        .expect("quad")
        .hard();
    let mut events = reopened(s.update());
    assert_eq!(events.len(), 1);
    let (Reopened::Tap(new), _) = events.remove(0) else {
        panic!("a tap")
    };
    assert_eq!(new.width(), 4);
    let _ = block(&mut s);
    let _ = s.update();
    assert!(drain(&mut old).1, "the first reader ends");
}
