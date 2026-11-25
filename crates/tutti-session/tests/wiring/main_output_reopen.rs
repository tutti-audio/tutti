//! A main output of another width opens again what reads the main output: a tap on it and
//! the main output meter are announced by `SessionEvent::MainOutputReaderReopened` with
//! the new topology and claimed with `Session::take_reopened`, the old reader
//! reads closed, and a reader that has the main output's width is left alone.
//!
//! Every figure is analytic: the source is the constant frame (0.25, 0.75), so
//! the stereo main output carries 0.25 and 0.75 and a mono main output keeps channel 0.

use tutti_buffers::{ChannelLayout, ChannelTopology, MaxBlock};
use tutti_buffers::{InterleavedMut, Pull, Source};
use tutti_graph::ForkByClone;
use tutti_graph::TapId;
use tutti_nodes::testing::Const;
use tutti_session::{
    MainOutput, MainOutputReader, NotReopened, OutputPort, Reopened, Session, SessionEvent,
};
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 64;

/// A stereo session whose main output is fed (0.25, 0.75), the plan installed.
fn session() -> Session {
    let mut s = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("builds");
    let node = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.75])));
    for c in 0..2 {
        s.graph()
            .set_sources(MainOutput::channel(c), [OutputPort::new(node, c)])
            .unwrap()
            .unfaded();
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
    tutti_buffers::Bed::Mono.topology()
}

/// The readers `events` announce.
fn reopened(events: Vec<SessionEvent>) -> Vec<(MainOutputReader, ChannelTopology)> {
    events
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::MainOutputReaderReopened { id, topology } => Some((id, topology)),
            _ => None,
        })
        .collect()
}

/// The tap `s` holds under `id`.
fn claim_tap(s: &mut Session, id: MainOutputReader) -> tutti_graph::TapReader {
    match s.take_reopened(id).expect("held") {
        Reopened::Tap(tap) => tap,
        other => panic!("a tap, not {other:?}"),
    }
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

/// **A main output change mid-stream reopens its tap once, with the new topology;
/// the old reader drains and reads closed; the new one reads the new width.**
/// The old reader holds the three blocks written before the commit, none lost;
/// the new reader holds the blocks from the commit's block on, one channel of
/// 0.25 (the mono main output keeps channel 0).
#[test]
fn a_main_output_width_change_reopens_the_main_output_tap_and_closes_the_old_reader() {
    let mut s = session();
    let mut old = s
        .graph()
        .taps()
        .open_main_output(Samples::new(4096))
        .expect("the outputs exist");
    assert_eq!(old.width(), 2);
    for _ in 0..3 {
        assert!(reopened(block(&mut s)).is_empty());
    }

    s.declare_main_output_topology(Some(mono()))
        .expect("a mono main output")
        .unfaded();
    let mut events = reopened(s.update());
    assert_eq!(events.len(), 1, "one reopen for the one tap");
    let (id, topology) = events.remove(0);
    assert_eq!(topology, mono(), "carries the new topology");
    assert_eq!(Some(id), old.id().map(MainOutputReader::Tap));
    let mut new = claim_tap(&mut s, id);
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
        "the old reader holds the stereo main output"
    );
    assert_eq!(
        old.take_lost(),
        Samples::new(0),
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
        "the mono main output keeps channel 0"
    );
    assert_eq!(new.take_lost(), Samples::new(0));
}

/// **A tap on a port whose width did not change is not reopened**: a
/// declaration of the stereo the main output already is changes the declaration and
/// not the width, so no event arrives and the reader keeps reading.
#[test]
fn a_declaration_that_keeps_the_width_leaves_the_tap_alone() {
    let mut s = session();
    let mut tap = s
        .graph()
        .taps()
        .open_main_output(Samples::new(4096))
        .expect("the outputs exist");
    let _ = block(&mut s);
    s.declare_main_output_topology(Some(tutti_buffers::Bed::Stereo.topology()))
        .expect("a stereo main output")
        .unfaded();
    for _ in 0..3 {
        assert!(reopened(block(&mut s)).is_empty());
    }
    let (frames, ended) = drain(&mut tap);
    assert!(!ended, "the reader is still open");
    assert_eq!(frames.len(), 4 * BLOCK * 2);
}

/// **A meter reopens with the new width**: the main output meter a host took
/// arrives with the new topology, measuring one channel at 0.25 (the peak and,
/// the source being constant, the rms); the old handle reads closed.
#[test]
fn the_taken_main_output_meter_reopens_with_the_new_width() {
    let mut s = session();
    let old = s.take_main_output_meter().expect("held");
    assert_eq!(old.channels(), 2);
    let _ = block(&mut s);
    assert!(!old.is_closed());

    s.declare_main_output_topology(Some(mono()))
        .expect("a mono main output")
        .unfaded();
    let mut events = reopened(block(&mut s));
    assert_eq!(events.len(), 1, "the meter, and no tap, was opened");
    let (id, topology) = events.remove(0);
    assert!(matches!(id, MainOutputReader::Meter(_)));
    assert_eq!(topology, mono());
    let Reopened::Meter(mut new) = s.take_reopened(id).expect("held") else {
        panic!("a meter")
    };
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

/// **A meter nobody took is replaced in place**: no event, and the graph's own
/// reader is the new one.
#[test]
fn a_meter_nobody_took_is_replaced_without_an_event() {
    let mut s = session();
    s.declare_main_output_topology(Some(mono()))
        .expect("a mono main output")
        .unfaded();
    assert!(reopened(block(&mut s)).is_empty());
    let _ = block(&mut s);
    let mut meter = s.take_main_output_meter().expect("held");
    assert_eq!(meter.channels(), 1);
    assert_eq!(meter.read()[0].peak.get(), 0.25);
}

/// **Only the last of two changes before an update is reported**, at the last
/// width, and the first reader never existed outside the graph.
#[test]
fn only_the_last_of_two_changes_before_an_update_is_reported() {
    let mut s = session();
    let mut old = s
        .graph()
        .taps()
        .open_main_output(Samples::new(4096))
        .expect("opens");
    let _ = block(&mut s);
    s.declare_main_output_topology(Some(mono()))
        .expect("mono")
        .unfaded();
    s.declare_main_output_topology(Some(
        ChannelTopology::default_for(ChannelLayout::from_count(4)).unwrap(),
    ))
    .expect("quad")
    .unfaded();
    let mut events = reopened(s.update());
    assert_eq!(events.len(), 1);
    let new = claim_tap(&mut s, events.remove(0).0);
    assert_eq!(new.width(), 4);
    let _ = block(&mut s);
    let _ = s.update();
    assert!(drain(&mut old).1, "the first reader ends");
}

/// **A second reopen before the claim replaces the held reader**: the one
/// announced first is gone, the claim is the last width, and the reader it
/// superseded ends.
#[test]
fn a_second_reopen_before_the_claim_replaces_the_held_reader() {
    let mut s = session();
    let _old = s
        .graph()
        .taps()
        .open_main_output(Samples::new(4096))
        .expect("opens");
    s.declare_main_output_topology(Some(mono()))
        .expect("mono")
        .unfaded();
    let first = reopened(block(&mut s));
    assert_eq!(first.len(), 1);
    s.declare_main_output_topology(Some(
        ChannelTopology::default_for(ChannelLayout::from_count(4)).unwrap(),
    ))
    .expect("quad")
    .unfaded();
    let second = reopened(block(&mut s));
    assert_eq!(second.len(), 1, "announced again");
    assert_eq!(first[0].0, second[0].0, "under the one id");
    assert_eq!(claim_tap(&mut s, second[0].0).width(), 4);
    assert_eq!(
        s.take_reopened(second[0].0).unwrap_err(),
        NotReopened(second[0].0),
        "claimed once"
    );
}

/// **An ignored event loses nothing**: the reader stays held through later
/// updates and is claimable then, with frames from the commit on.
#[test]
fn an_unclaimed_reader_survives_ignored_updates_and_is_claimed_later() {
    let mut s = session();
    let _old = s
        .graph()
        .taps()
        .open_main_output(Samples::new(4096))
        .expect("opens");
    s.declare_main_output_topology(Some(mono()))
        .expect("mono")
        .unfaded();
    let events = reopened(block(&mut s));
    assert_eq!(events.len(), 1);
    for _ in 0..3 {
        assert!(reopened(block(&mut s)).is_empty(), "announced once");
    }
    let mut late = claim_tap(&mut s, events[0].0);
    assert_eq!(late.width(), 1);
    let (frames, _) = drain(&mut late);
    assert_eq!(frames.len(), 4 * BLOCK, "every block since the commit");
    assert!(frames.iter().all(|&x| x == 0.25));
}

/// **A claim of an id nothing was reopened under is an error at the call.**
#[test]
fn a_claim_of_an_unknown_id_is_refused() {
    let mut s = session();
    let id = MainOutputReader::Tap(TapId::of(99));
    assert_eq!(s.take_reopened(id).unwrap_err(), NotReopened(id));
}

/// **A widening is a declaration, so it reopens the readers at the new width**:
/// a 2-wide tap, the main output widened to 4, reopens at 4 (the reader a claim
/// takes is 4 wide), and declaring that width's order again moves nothing.
#[test]
fn a_widening_reopens_the_tap_at_the_new_width_and_a_declaration_of_it_leaves_it() {
    let mut s = session();
    let old = s
        .graph()
        .taps()
        .open_main_output(Samples::new(4096))
        .expect("opens");
    s.graph().widen_outputs(4).expect("widens");
    let four = tutti_buffers::Bed::Quad.topology();
    let mut events = reopened(block(&mut s));
    events.retain(|(id, _)| matches!(id, MainOutputReader::Tap(_)));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, four);
    assert_eq!(Some(events[0].0), old.id().map(MainOutputReader::Tap));
    assert_eq!(claim_tap(&mut s, events[0].0).width(), 4);

    s.declare_main_output_topology(Some(four))
        .expect("quad")
        .unfaded();
    assert!(
        reopened(block(&mut s))
            .iter()
            .all(|(id, _)| matches!(id, MainOutputReader::Meter(_))),
        "the width is the main output's: no tap is reopened"
    );
}

/// **Closing a tap drops the reader held for it**: the claim is refused, and
/// the reader the reopen superseded reads `Ended`.
#[test]
fn closing_a_tap_drops_the_reader_held_for_it() {
    let mut s = session();
    let mut old = s
        .graph()
        .taps()
        .open_main_output(Samples::new(4096))
        .expect("opens");
    let id = old.id().expect("named");
    let _ = block(&mut s);
    s.declare_main_output_topology(Some(mono()))
        .expect("mono")
        .unfaded();
    assert_eq!(reopened(block(&mut s)).len(), 1);
    s.graph().taps().close(id).expect("open");
    let _ = block(&mut s);
    let _ = s.update();
    assert_eq!(
        s.take_reopened(MainOutputReader::Tap(id)).unwrap_err(),
        NotReopened(MainOutputReader::Tap(id))
    );
    assert!(drain(&mut old).1, "the superseded reader ends");
}

/// **Closing the main output meter drops the reader held for it**: the claim is
/// refused, and the superseded handle reads closed.
#[test]
fn closing_the_main_output_meter_drops_the_reader_held_for_it() {
    let mut s = session();
    let old = s.take_main_output_meter().expect("held");
    let id = old.id();
    let _ = block(&mut s);
    s.declare_main_output_topology(Some(mono()))
        .expect("mono")
        .unfaded();
    assert_eq!(reopened(block(&mut s)).len(), 1);
    s.graph().meters().close(id).expect("open");
    for _ in 0..2 {
        let _ = block(&mut s);
    }
    assert_eq!(
        s.take_reopened(MainOutputReader::Meter(id)).unwrap_err(),
        NotReopened(MainOutputReader::Meter(id))
    );
    assert!(old.is_closed());
}
