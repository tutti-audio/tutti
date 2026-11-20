//! A root declared while a root crossfade runs waits for it: the declaration,
//! what reads the root and the audio change together, in the first update
//! after the running fade has ended, to the newest declaration.
//!
//! Every figure is analytic: constants of exactly representable values and
//! the equal-amplitude gains `(1 - s, s)` with the smootherstep
//! `s(x) = x^3 (6x^2 - 15x + 10)` at `x = (k + 1) / (n + 1)`, `n` the 240
//! frames of 5 ms at 48 kHz.

use tutti_buffers::Speaker;
use tutti_buffers::{BusTopology, ChannelLayout, ChannelTopology, MaxBlock};
use tutti_buffers::{InterleavedMut, Pull, Source};
use tutti_graph::Unforkable;
use tutti_graph::{BusRef, BusSource, Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::Const;
use tutti_session::{OutPort, Reopened, Root, RootReader, Session, SessionEvent};
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 64;
const FADE: usize = 240;

/// A stereo session whose root is fed (0.25, 0.75), the plan installed.
fn session() -> (Session, tutti_session::Background) {
    let (mut s, bg) = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("builds");
    let (node, _) = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.75])));
    for c in 0..2 {
        s.graph()
            .set_sources_unfaded(Root::output(c), [OutPort::new(node, c)])
            .unwrap();
    }
    let _ = s.update();
    (s, bg)
}

/// One update, then one block rendered; the update's events and the block.
fn block(s: &mut Session) -> (Vec<SessionEvent>, Vec<[f32; 2]>) {
    let events = s.update();
    let mut out = vec![f32::NAN; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    (events, out.as_chunks::<2>().0.to_vec())
}

fn root(n: u16) -> ChannelTopology {
    ChannelTopology::assumed(ChannelLayout::from_count(n))
}

fn mono() -> ChannelTopology {
    root(1)
}

fn smootherstep(x: f64) -> f64 {
    x * x * x * (6.0 * x * x - 15.0 * x + 10.0)
}

/// `(out, in)` at frame `k` of a fade.
fn gains(k: usize) -> (f32, f32) {
    if k >= FADE {
        return (0.0, 1.0);
    }
    let s = smootherstep((k + 1) as f64 / (FADE + 1) as f64);
    ((1.0 - s) as f32, s as f32)
}

fn reopened(events: &[SessionEvent]) -> Vec<(RootReader, ChannelTopology)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::RootReaderReopened { id, topology } => Some((*id, topology.clone())),
            _ => None,
        })
        .collect()
}

fn claim_tap(s: &mut Session, id: RootReader) -> tutti_graph::TapReader {
    match s.take_reopened(id).expect("held") {
        Reopened::Tap(tap) => tap,
        other => panic!("a tap, not {other:?}"),
    }
}

/// Everything `tap` holds, interleaved, and whether it ended.
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

/// The fade a mono declaration starts: the right is `0.75 (1 - s) + 0.25 s`.
fn mono_fade_right(k: usize) -> f32 {
    let (o, n) = gains(k);
    0.75 * o + 0.25 * n
}

/// Declares mono, takes the reopened tap, and renders one block: the fade is
/// running, 64 frames of it done.
fn start_fade(s: &mut Session) -> tutti_graph::TapReader {
    let mut tap = s
        .graph()
        .taps()
        .open_output(Samples::new(8192))
        .expect("opens");
    assert_eq!(tap.width(), 2);
    s.declare_root_topology(Some(mono())).expect("mono");
    let (events, frames) = block(s);
    let mut opened = reopened(&events);
    assert_eq!(opened.len(), 1, "the mono root reopens the tap");
    let new = claim_tap(s, opened.remove(0).0);
    assert_eq!(new.width(), 1);
    assert!(drain(&mut tap).1, "the stereo reader reads closed");
    for (k, f) in frames.iter().enumerate() {
        assert!((f[1] - mono_fade_right(k)).abs() < 1e-6, "frame {k}");
    }
    new
}

/// **A root of another width, declared mid-fade, changes nothing before the
/// running fade ends; after, the declaration, the announcement, the tap and
/// the audio change in one update.** The right of the audio is the mono
/// fade's until frame 240, the quad root (channel 0 only: 0.25 on the left
/// of the device, nothing on the right) fades in from there, and the new tap
/// holds exactly the frames rendered since the announcement.
#[test]
fn a_redeclare_of_another_width_mid_fade_applies_with_everything_that_reads_the_root() {
    let (mut s, _bg) = session();
    let mut mono_tap = start_fade(&mut s);
    s.declare_root_topology(Some(root(4))).expect("quad");
    assert_eq!(s.requested_root_topology(), Some(&root(4)));
    assert_eq!(s.root_topology(), Some(&mono()), "the root in force");
    let mut played = BLOCK;
    // Frames 64..256: the fade ends in the 4th block, and the update after it
    // applies the declaration.
    for _ in 0..3 {
        let (events, frames) = block(&mut s);
        assert!(
            reopened(&events).is_empty(),
            "no announcement before the end"
        );
        assert_eq!(s.graph_ref().outputs(), 1, "the plan's width is the mono's");
        assert_eq!(s.root_topology(), Some(&mono()));
        for (i, f) in frames.iter().enumerate() {
            let k = played + i;
            assert!((f[1] - mono_fade_right(k)).abs() < 1e-6, "frame {k}");
        }
        played += BLOCK;
    }
    let (_, rest) = (0, drain(&mut mono_tap));
    assert!(!rest.1, "the mono reader is still open");
    assert_eq!(rest.0.len(), 4 * BLOCK, "and holds the frames played");
    assert!(rest.0.iter().all(|&x| x == 0.25));

    let (events, frames) = block(&mut s);
    let mut opened = reopened(&events);
    assert_eq!(
        opened.len(),
        1,
        "one announcement, at the update that applies"
    );
    let (id, topology) = opened.remove(0);
    assert_eq!(topology, root(4));
    assert_eq!(
        s.graph_ref().outputs(),
        4,
        "the width changed in that update"
    );
    assert_eq!(s.root_topology(), Some(&root(4)));
    let mut quad_tap = claim_tap(&mut s, id);
    assert_eq!(quad_tap.width(), 4);
    // The quad fade renders from this block: frame k of it.
    for (k, f) in frames.iter().enumerate() {
        let (o, n) = gains(k);
        assert!((f[0] - 0.25).abs() < 1e-6, "left, frame {k}");
        assert!(
            (f[1] - (0.25 * o + 0.0 * n)).abs() < 1e-6,
            "right, frame {k}: {} against {}",
            f[1],
            0.25 * o
        );
    }
    for _ in 0..4 {
        let _ = block(&mut s);
    }
    assert!(drain(&mut mono_tap).1, "the mono reader reads closed");
    let (heard, ended) = drain(&mut quad_tap);
    assert!(!ended);
    assert_eq!(
        heard.len(),
        5 * BLOCK * 4,
        "the frames since the announcement"
    );
    assert!(
        heard
            .as_chunks::<4>()
            .0
            .iter()
            .all(|f| f[0] == 0.25 && f[1..] == [0.0, 0.0, 0.0]),
        "the tap holds the quad root the audio is"
    );
}

/// **Two declarations during one fade give one applied, the newest**, and the
/// audio after it is the newest root's fade-in: the 5.1 root keeps channel 0
/// (0.25) on the left and nothing on the right, which fades in from the mono
/// root's 0.25.
#[test]
fn two_redeclares_mid_fade_apply_the_newest_once() {
    let (mut s, _bg) = session();
    let _tap = start_fade(&mut s);
    s.declare_root_topology(Some(root(4))).expect("quad");
    let _ = block(&mut s);
    s.declare_root_topology(Some(root(6))).expect("5.1");
    assert_eq!(s.requested_root_topology(), Some(&root(6)));
    let mut announced = Vec::new();
    let mut applied = Vec::new();
    for _ in 0..8 {
        let (events, frames) = block(&mut s);
        let now = reopened(&events);
        if !now.is_empty() {
            applied.clear();
        }
        announced.extend(now);
        applied.extend(frames);
    }
    assert_eq!(announced.len(), 1, "one declaration was applied");
    assert_eq!(announced[0].1, root(6));
    assert_eq!(s.graph_ref().outputs(), 6);
    for (k, f) in applied.iter().enumerate() {
        let (o, _) = gains(k);
        assert!(
            (f[0] - 0.25).abs() < 1e-6 && (f[1] - 0.25 * o).abs() < 1e-6,
            "frame {k} of the newest root's fade-in: {f:?}"
        );
    }
}

/// **Waiting is a status**: `Pending` while the declaration waits, with the
/// request the waiting one, `Applied` once it is in force.
#[test]
fn a_waiting_declaration_is_pending_until_it_applies() {
    use tutti_session::RootTopologyStatus;
    let (mut s, _bg) = session();
    let _tap = start_fade(&mut s);
    assert_eq!(s.root_topology_status(), RootTopologyStatus::Applied);
    s.declare_root_topology(Some(root(4))).expect("quad");
    assert_eq!(s.root_topology_status(), RootTopologyStatus::Pending);
    assert_eq!(s.requested_root_topology(), Some(&root(4)));
    let _ = block(&mut s);
    assert_eq!(s.root_topology_status(), RootTopologyStatus::Pending);
    for _ in 0..4 {
        let _ = block(&mut s);
    }
    assert_eq!(s.root_topology_status(), RootTopologyStatus::Applied);
    assert_eq!(s.root_topology(), Some(&root(4)));
}

/// **A declaration refused at its call drops nothing**: the one waiting still
/// applies, and the refusal is the status of the last request.
#[test]
fn a_refused_newer_declaration_leaves_the_waiting_one_to_apply() {
    use tutti_session::RootTopologyStatus;
    let (mut s, _bg) = session();
    let _tap = start_fade(&mut s);
    s.declare_root_topology(Some(root(4))).expect("quad");
    let buses = ChannelTopology::unassigned(ChannelLayout::from_count(4));
    assert!(
        s.declare_root_topology(Some(buses.clone())).is_err(),
        "four buses have no place on a stereo device"
    );
    assert!(matches!(
        s.root_topology_status(),
        RootTopologyStatus::Refused(_)
    ));
    for _ in 0..5 {
        let _ = block(&mut s);
    }
    assert_eq!(s.root_topology(), Some(&root(4)), "the waiting one applied");
    assert_eq!(s.graph_ref().outputs(), 4);
    assert_eq!(s.requested_root_topology(), Some(&buses));
}

/// **A `.unfaded()` declaration mid-fade applies at once** and drops a waiting
/// one: the announcement is that update's, and the next block is the new root
/// alone.
#[test]
fn a_hard_redeclare_mid_fade_applies_at_once_and_drops_the_waiting_one() {
    let (mut s, _bg) = session();
    let _tap = start_fade(&mut s);
    s.declare_root_topology(Some(root(6))).expect("5.1");
    s.declare_root_topology(Some(root(4)))
        .expect("quad")
        .unfaded();
    assert_eq!(s.root_topology(), Some(&root(4)), "in force at once");
    let (events, frames) = block(&mut s);
    assert_eq!(reopened(&events).len(), 1);
    assert_eq!(s.graph_ref().outputs(), 4);
    assert!(
        frames.iter().all(|f| f[0] == 0.25 && f[1] == 0.0),
        "the quad root alone from the commit's block"
    );
    for _ in 0..8 {
        assert!(
            reopened(&block(&mut s).0).is_empty(),
            "the 5.1 never applies"
        );
    }
    assert_eq!(s.graph_ref().outputs(), 4);
}

// ---------------------------------------------------------------------------
// A root fed by a bus.
// ---------------------------------------------------------------------------

/// Constant `values` on unassigned output channels.
struct Src {
    values: Vec<f32>,
    out: BusTopology,
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::source(self.out)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (c, &v) in self.values.iter().enumerate() {
            io.output(c)[0] = v;
        }
        Status::Constant
    }
    fn reset(&mut self) {}
}

fn swapped() -> ChannelTopology {
    ChannelTopology::new([Speaker::FrontRight, Speaker::FrontLeft]).unwrap()
}

/// **A bus-fed root re-declared mid-fade keeps the old root's fold until the
/// fade ends.** A two-channel source (0.25, 0.75) feeds a stereo root through
/// a bus edge; the root is declared as two independent buses (the fade: the
/// same sound), and, mid-fade, with the front pair swapped. The sound is
/// (0.25, 0.75) for the 240 frames of the first fade; the swap then fades in
/// by the law: `left = 0.25 (1 - s) + 0.75 s`, `right = 0.75 (1 - s) + 0.25 s`.
#[test]
fn a_bus_fed_root_redeclared_mid_fade_keeps_its_fold_until_the_fade_ends() {
    let (mut s, _bg) = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .root_topology(root(2))
        .build()
        .expect("builds");
    let (src, _) = s.graph().add(Unforkable(Src {
        values: vec![0.25, 0.75],
        out: BusTopology::unassigned(ChannelLayout::STEREO).unwrap(),
    }));
    s.graph()
        .connect(BusSource::Node(BusRef::new(src.erase(), 0)), Root)
        .expect("folds by index")
        .unfaded();
    let _ = block(&mut s);
    s.declare_root_topology(Some(ChannelTopology::unassigned(ChannelLayout::STEREO)))
        .expect("buses");
    let mut first = block(&mut s).1;
    s.declare_root_topology(Some(swapped())).expect("swapped");
    for _ in 0..3 {
        first.extend(block(&mut s).1);
    }
    for (k, f) in first.iter().enumerate() {
        assert!(
            (f[0] - 0.25).abs() < 1e-6 && (f[1] - 0.75).abs() < 1e-6,
            "frame {k}: the old fold until the fade ends, {f:?}"
        );
    }
    let mut second = Vec::new();
    for _ in 0..5 {
        second.extend(block(&mut s).1);
    }
    for (k, f) in second.iter().enumerate() {
        let (o, n) = gains(k);
        let (left, right) = (0.25 * o + 0.75 * n, 0.75 * o + 0.25 * n);
        assert!(
            (f[0] - left).abs() < 1e-6 && (f[1] - right).abs() < 1e-6,
            "frame {k}: {f:?} against ({left}, {right})"
        );
    }
}
