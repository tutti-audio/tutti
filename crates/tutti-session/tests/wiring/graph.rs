//! The session's graph properties, with no host: the edit flag, the
//! replacement retry, the event feeds, compensation, and what a fork renders.
//!
//! Design 015 §9.4.

use std::sync::Arc;
use tutti_buffers::MaxBlock;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MAX_CHANNELS;
use tutti_graph::Params;
use tutti_graph::{
    Cx, Fade, ForkByClone, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape,
    Status, Unforkable,
};
use tutti_nodes::testing::{Const, Osc, Through};
use tutti_rt::Param;
use tutti_session::{
    DetachedSession, EventInPort, Feeder, InPort, OutPort, RemoveError, ReplaceRefused,
    Replacement, Root, Session, SessionEvent, SessionNode, root_width,
};
use tutti_units::{Drive, Hz, Real, SampleRate, Samples};
use tutti_units::{Latency, UnitParam};

const RATE: SampleRate = SampleRate::SR_48K;

/// A detached session `outputs` wide at 48 kHz: the executor on this side.
fn detached(outputs: usize) -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::clamped(outputs))
        .build_detached(RATE)
        .expect("builds")
}

/// The session is `Send`: a host may build it on one thread and hand it to
/// its main thread (design 015 §4.2). The `Device`'s `!Send` is its own
/// `compile_fail` doctest.
#[test]
fn a_session_is_send() {
    fn send<T: Send>() {}
    send::<Session>();
}

/// An app's own key type names a session, and `Send` follows the clock alone.
#[test]
fn a_session_takes_an_app_key_type() {
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct MyKey(u32);
    fn assert_send<T: Send>() {}
    assert_send::<Session<MyKey, tutti_session::Offline>>();
}

// ---- stimulus nodes ---------------------------------------------------------

/// A source whose one output is its `Drive` param: its drive a `Param` cell
/// addressed by a `ParamSet`, inserted through `param_parts` (so its fork
/// starts from the values its cells hold).
#[derive(Clone)]
struct Knob {
    drive: Param<Drive>,
    /// Held to see when the node is dropped (a test counts the owners).
    _token: Option<std::sync::Arc<()>>,
}

impl Knob {
    const BUILT_WITH: f64 = 1.0;

    fn new() -> Self {
        Self::at(Self::BUILT_WITH)
    }

    fn at(drive: f64) -> Self {
        Self {
            drive: Param::new(Drive::new(drive)),
            _token: None,
        }
    }

    fn holding(mut self, token: &std::sync::Arc<()>) -> Self {
        self._token = Some(std::sync::Arc::clone(token));
        self
    }
}

impl Node for Knob {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_params(&[UnitParam::Drive])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match self
            .declared_param::<{ UnitParam::Drive as u16 }, _>(&io)
            .frames()
        {
            Some(v) => {
                let v: Vec<f32> = v.to_vec();
                io.output(0).copy_from_slice(&v);
            }
            None => io.output(0).fill(f32::from_unit(self.drive.load())),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        (port == 0).then(|| self.drive.load().get())
    }
}

impl ParamNode for Knob {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.drive.detach();
        fork
    }
}

tutti_graph::declare_params!([] Knob, n { Drive => Drive = &n.drive });

/// A source with an event output, which no knob has: another shape. It holds
/// a token to see when the node is dropped.
#[derive(Clone)]
struct Holder(#[expect(dead_code, reason = "held to be counted")] std::sync::Arc<()>);

impl Node for Holder {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A [`Knob`] of another type: the same source and the same param, under a
/// type of its own, so a replacement of one by the other changes the node's
/// type and keeps its shape.
#[derive(Clone)]
struct Dial(Knob);

impl Dial {
    fn at(drive: f64) -> Self {
        Self(Knob::at(drive))
    }
}

impl Node for Dial {
    fn shape(&self) -> Shape {
        self.0.shape()
    }
    fn prepare(&mut self, prepare: &Prepare) {
        self.0.prepare(prepare);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.process(cx, io)
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.0.param_base(port)
    }
}

impl ParamNode for Dial {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        Self(self.0.fork_fresh())
    }
}

tutti_graph::declare_params!([] Dial, n { Drive => Drive = &n.0.drive });

/// A node that cannot run above 90 kHz: its `prepare` panics there, which is
/// how a re-prepare poisons an editor.
struct Grenade;

impl Node for Grenade {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, prepare: &Prepare) {
        assert!(prepare.sample_rate().get() < 90_000.0, "no such rate");
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A pure delay of `N` frames that declares `N` frames of latency: what a
/// lookahead is to PDC, with an exact figure.
#[derive(Clone)]
struct Late<const N: usize> {
    ring: Vec<f32>,
    at: usize,
}

impl<const N: usize> Late<N> {
    fn new() -> Self {
        Self {
            ring: vec![0.0; N],
            at: 0,
        }
    }
}

impl<const N: usize> Node for Late<N> {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO).with_latency(Latency::new(Samples::new(N)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (i, mut o) = io.split();
        for (y, &x) in o.get(0).iter_mut().zip(i.get(0)) {
            *y = self.ring[self.at];
            self.ring[self.at] = x;
            self.at = (self.at + 1) % N;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.at = 0;
    }
}

/// A node with `IN` event inputs and `OUT` event outputs and nothing else:
/// a sink or a source of event edges.
#[derive(Clone)]
struct Events<const IN: u16, const OUT: u16>;

impl<const IN: u16, const OUT: u16> Node for Events<IN, OUT> {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(IN, OUT)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---- the graph's own vocabulary ----------------------------------------------

/// Every [`OutPort`] kind survives a write and a read-back, on a node
/// port and on a global output: the translation to the spec's spelling and
/// back is total and lossless.
#[test]
fn every_source_reads_back_as_written() {
    let mut s = Session::builder()
        .inputs(ChannelLayout::MONO)
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("builds");
    let graph = s.graph();
    let stereo = graph.add(ForkByClone(Const::frame(&[1.0, 2.0])));
    let sink = graph.add(ForkByClone(Through::mono()));
    for source in [
        OutPort::new(stereo, 1),
        OutPort::new(stereo, 0),
        Root::input(0),
    ] {
        graph
            .set_sources(InPort::new(sink, 0), [source])
            .unwrap()
            .unfaded();
        let held = vec![source];
        assert_eq!(
            graph.sources(InPort::new(sink, 0)).unwrap(),
            held,
            "node port"
        );
        graph
            .set_sources(Root::output(1), [source])
            .unwrap()
            .unfaded();
        assert_eq!(
            graph.sources(Root::output(1)).unwrap(),
            held,
            "global output"
        );
    }
}

/// Silence is stored as the empty set, never as a `PortSource::Zero` (which the
/// compiler's validation refuses), whichever call wrote it: a port or output
/// set to no sources, a cleared sink, and a fresh or widened root. `planned_compensation` is `None` when the spec does not validate.
#[test]
fn every_setter_stores_silence_as_the_empty_set() {
    let mut s = detached(2);
    let graph = s.graph();
    let sink = graph.add(ForkByClone(Through::mono()));
    let src = graph.add(ForkByClone(Const::frame(&[1.0, 2.0])));
    assert!(
        graph.testing().planned_compensation().is_some(),
        "fresh root"
    );

    graph
        .set_sources(InPort::new(sink, 0), [OutPort::new(src, 0)])
        .unwrap()
        .unfaded();
    graph
        .set_sources(InPort::new(sink, 0), [])
        .unwrap()
        .unfaded();
    assert!(
        graph.testing().planned_compensation().is_some(),
        "set_sources of none"
    );

    graph
        .set_sources(Root::output(0), [OutPort::new(src, 0)])
        .unwrap()
        .unfaded();
    graph.set_sources(Root::output(0), []).unwrap().unfaded();
    assert!(
        graph.testing().planned_compensation().is_some(),
        "set_sources of none after one"
    );

    graph.connect(src, Root).unwrap().unfaded();
    graph.disconnect(src, Root).unwrap();
    assert!(
        graph.testing().planned_compensation().is_some(),
        "disconnected main bus"
    );

    graph.widen_outputs(4).expect("widens");
    assert!(
        graph.testing().planned_compensation().is_some(),
        "widened root"
    );
    assert_eq!(graph.sources(Root::output(3)).unwrap(), []);
}

/// `remove` says whether the node was there, so removing a node twice is a
/// no-op rather than a panic.
#[test]
fn removing_a_node_twice_names_what_was_not_there() {
    let mut s = detached(1);
    let node = s.graph().add(ForkByClone(Const::mono(1.0)));
    assert!(s.graph_ref().contains(node));
    assert!(s.remove(node).is_ok());
    assert!(!s.graph_ref().contains(node));
    assert_eq!(
        s.remove(node).map(|_| ()),
        Err(RemoveError::NoSuchNode(node.erase())),
        "the remove names the node that is gone"
    );
}

/// A node that was never in the graph is refused by name, and the refusal
/// changes nothing: the graph is not marked edited.
#[test]
fn removing_a_stranger_changes_nothing() {
    let mut s = detached(1);
    let _ = s.update();
    let stranger = tutti_graph::NodeKey(u64::MAX);
    assert_eq!(
        s.remove(stranger).map(|_| ()),
        Err(RemoveError::NoSuchNode(stranger))
    );
    assert!(!s.graph_ref().has_uncommitted_edits(), "nothing was edited");
}

/// `root_width` is `max(project, device)` clamped to
/// `1..=MAX_CHANNELS`; each row is one way the rule is load-bearing.
#[test]
fn root_width_takes_the_wider_side_within_the_scratch_bounds() {
    let cases: &[(ChannelLayout, ChannelLayout, usize, &str)] = &[
        (
            ChannelLayout::STEREO,
            ChannelLayout::from(6u16),
            6,
            "a wider device widens the root: a stereo project on a 5.1 device \
             must render all six, or the top four are permanently silent",
        ),
        (
            ChannelLayout::from_count(6),
            ChannelLayout::STEREO,
            6,
            "a wider project is kept and folded, not clamped",
        ),
        (
            ChannelLayout::EMPTY,
            ChannelLayout::from(6u16),
            6,
            "an unset project width falls through to the device",
        ),
        (
            ChannelLayout::EMPTY,
            ChannelLayout::STEREO,
            2,
            "an unset project width falls through to the device",
        ),
        (
            ChannelLayout::from_count(64),
            ChannelLayout::from(32u16),
            MAX_CHANNELS,
            "nothing exceeds the render scratch",
        ),
        (
            ChannelLayout::EMPTY,
            ChannelLayout::EMPTY,
            1,
            "the root is never zero wide: a zero-output root would render \
             nothing at all",
        ),
    ];
    for &(project, device, expected, why) in cases {
        assert_eq!(
            root_width(project, device),
            expected,
            "root_width({} channels, {} channels): {why}",
            project.count(),
            device.count()
        );
    }
}

// ---- the edit flag ------------------------------------------------------------

/// **An edit between two updates is committed by the next update.** One
/// commit, and the plan it sends is a new one.
#[test]
fn an_edit_between_updates_is_committed_by_the_next() {
    let mut s = detached(1);
    let a = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(a, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    let sent = s.graph_ref().testing().commits_sent();
    let plan = s
        .graph_ref()
        .testing()
        .sent_plan()
        .expect("the first update sent a plan");

    let b = s.graph().add(ForkByClone(Const::mono(2.0)));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(b, 0)])
        .unwrap()
        .unfaded();
    assert!(s.graph_ref().has_uncommitted_edits());
    let _ = s.update();
    assert!(
        !s.graph_ref().has_uncommitted_edits(),
        "the update committed the edit"
    );
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        sent + 1,
        "one commit"
    );
    let after = s.graph_ref().testing().sent_plan().expect("a plan");
    assert!(!Arc::ptr_eq(&plan, &after), "and it sent a new plan");
    let mut out = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(out[0], 2.0, "the edit is what renders");
}

/// **An update with no edit sends no plan**: the plan the audio side holds
/// is the same `Arc`, and no commit was counted.
#[test]
fn an_update_with_no_edit_sends_no_plan() {
    let mut s = detached(1);
    let a = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(a, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    let sent = s.graph_ref().testing().commits_sent();
    let plan = s
        .graph_ref()
        .testing()
        .sent_plan()
        .expect("the first update sent a plan");
    for _ in 0..3 {
        let _ = s.update();
    }
    assert_eq!(s.graph_ref().testing().commits_sent(), sent, "no commit");
    let after = s.graph_ref().testing().sent_plan().expect("a plan");
    assert!(Arc::ptr_eq(&plan, &after), "the same plan");
}

// ---- replacements ---------------------------------------------------------------

/// **A replace on a node that is gone is refused with the node named**, not
/// with a message to parse: the caller matches `NoSuchNode(node)`.
#[test]
fn a_replace_on_a_removed_node_is_refused_naming_it() {
    let mut s = detached(1);
    let knob = s.graph().add(Params(Knob::new()));
    s.remove(knob).unwrap();
    let refused = s
        .graph()
        .replace(knob, Replacement::new(Params(Knob::at(3.0))))
        .expect_err("the node is gone");
    assert_eq!(refused, ReplaceRefused::NoSuchNode(knob.erase()));
}

/// **A replace during a re-prepare lands on a later update**, its params
/// addressed under the key: the graph an engine runs (the audio side is
/// taken) changing rate, and a replace before the executor has handed its
/// units back. The call returns the key; the update that lands it sends
/// `Replaced`.
///
/// `Editor::replace` refuses while re-preparing and consumes its unit, so
/// without the wait the new unit would be lost.
#[test]
fn a_replace_during_a_re_prepare_lands_on_a_later_update() {
    let mut s = detached(1);
    let mut side = s.graph().take_audio_side();
    let knob = s.graph().add(Params(Knob::new()));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(knob, 0)])
        .unwrap()
        .unfaded();
    assert!(s.update().is_empty());
    let mut out = [0.0f32];
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(f64::from(out[0]), Knob::BUILT_WITH, "the knob plays");

    // The rate changes on the running graph: the first half is sent, and
    // the executor has not run it yet.
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let replaced = s
        .graph()
        .replace(knob, Replacement::new(Params(Knob::at(3.0))))
        .expect("the node is in the graph");
    assert_eq!(replaced, knob, "the key names the incoming node at once");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 1);

    // Update by update through the re-prepare: the executor checks its units
    // out (a silent block), the editor sends them back, the executor
    // resumes. The replacement waits until an update finds the graph resumed.
    assert!(s.update().is_empty());
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert!(s.update().is_empty());
    s.set(knob.drive().to(Drive::new(7.0))).unwrap();
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(
        f64::from(out[0]),
        Knob::BUILT_WITH,
        "the old unit plays on, as it was: the write is the incoming node's"
    );

    let events = s.update();
    match events.as_slice() {
        [SessionEvent::Replaced { node }] => assert_eq!(*node, knob.erase()),
        other => panic!("landed once the graph resumed: {other:?}"),
    }
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);
    for _ in 0..512 {
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert_eq!(
        out[0], 7.0,
        "the incoming knob sounds with what was written to its key while it waited"
    );
    s.set(knob.drive().to(Drive::new(9.0))).unwrap();
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(out[0], 9.0, "its params are addressed under the key");
}

/// **Removing a node purges the replacements parked on it**: the graph
/// re-prepares, a replacement waits, the node is removed, and the later updates
/// report no `ReplaceFailed` for it (before, the retry failed `NoSuchNode`
/// and dropped the incoming node then); the incoming node is dropped at the
/// removal.
#[test]
fn removing_a_node_purges_the_replacements_parked_on_it() {
    use std::sync::Arc;

    let mut s = detached(1);
    let mut side = s.graph().take_audio_side();
    let knob = s.graph().add(Params(Knob::new()));
    assert!(s.update().is_empty());
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let token = Arc::new(());
    s.graph()
        .replace(
            knob,
            Replacement::new(Params(Knob::at(3.0).holding(&token))),
        )
        .expect("the node is in the graph");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 1);

    s.remove(knob).unwrap();
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0, "purged");
    assert_eq!(Arc::strong_count(&token), 1, "the incoming node dropped");

    let mut out = [0.0f32];
    for _ in 0..8 {
        let events = s.update();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::ReplaceFailed { .. })),
            "{events:?}"
        );
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
}

/// A detached session with a knob playing at the root, its first plan sent,
/// and the audio side taken: the running node a swap fades from.
fn playing_knob() -> (
    DetachedSession,
    tutti_session::AudioSide,
    tutti_session::Key<Params<Knob>>,
) {
    let mut s = detached(1);
    let mut side = s.graph().take_audio_side();
    let knob = s.graph().add(Params(Knob::new()));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(knob, 0)])
        .unwrap()
        .unfaded();
    assert!(s.update().is_empty());
    let mut out = [0.0f32];
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(f64::from(out[0]), Knob::BUILT_WITH, "the knob plays");
    (s, side, knob)
}

/// **A fade over a node of another shape is refused, and a zero fade cuts**:
/// the default declick is not zero, so a default `Replacement::new` across a
/// shape change is `ShapeChanged` naming the node and both shapes, with the
/// running node, its values and its controls untouched; the same swap with a
/// zero-length fade lands as a plain swap, and the incoming node sounds from
/// the first frame after the commit.
#[test]
fn a_fade_over_a_shape_change_is_refused_and_a_zero_fade_cuts() {
    let (mut s, mut side, knob) = playing_knob();
    let refused = s
        .graph()
        .replace(knob, Replacement::new(ForkByClone(Events::<0, 1>)))
        .expect_err("the shape differs and the default fade is not zero");
    let ReplaceRefused::ShapeChanged {
        node,
        running,
        incoming,
    } = refused
    else {
        panic!("{refused:?}");
    };
    assert_eq!(node, knob.erase());
    assert_eq!(running.event_out, 0);
    assert_eq!(incoming.event_out, 1);
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);

    // Nothing changed: the knob is still the node, still steerable.
    s.set(knob.drive().to(Drive::new(4.0))).unwrap();
    assert!(s.update().is_empty());
    let mut out = [0.0f32];
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(out[0], 4.0, "the refused swap left the running knob");

    let cut = Fade::new(Samples::ZERO);
    s.graph()
        .replace(
            knob,
            Replacement::new(ForkByClone(Events::<0, 1>)).fade(cut),
        )
        .expect("a cut over a shape change is accepted");
    assert!(s.update().is_empty());
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(out[0], 0.0, "the incoming node, with no blend of the knob");
}

/// **A parked swap checks the shapes when it lands**: a re-prepare is between
/// its commits, a fade is asked over another shape, and the update that finds
/// the graph resumed reports `ReplaceFailed { ShapeChanged }`. The incoming
/// node is dropped, and the key gives the outgoing knob its values back: a
/// write through it reaches the knob that is still playing.
#[test]
fn a_parked_swap_over_a_shape_change_fails_when_it_lands() {
    use std::sync::Arc;

    let (mut s, mut side, knob) = playing_knob();
    let token = Arc::new(());
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    s.graph()
        .replace(
            knob,
            Replacement::new(ForkByClone(Holder(Arc::clone(&token)))),
        )
        .expect("accepted: the shapes are checked when it lands");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 1);
    assert!(Arc::strong_count(&token) > 1, "the incoming node waits");

    let mut out = [0.0f32];
    let mut failed = None;
    for _ in 0..8 {
        for event in s.update() {
            if let SessionEvent::ReplaceFailed { node, refused } = event {
                failed = Some((node, refused));
            }
        }
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    let (node, refused) = failed.expect("the retry was refused");
    assert_eq!(node, knob.erase());
    assert!(
        matches!(refused, ReplaceRefused::ShapeChanged { .. }),
        "{refused:?}"
    );
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);
    assert_eq!(Arc::strong_count(&token), 1, "the incoming node is dropped");
}

/// **A parked swap with a zero fade lands over another shape**, as the same
/// swap does at once.
#[test]
fn a_parked_swap_with_a_zero_fade_lands_over_a_shape_change() {
    let (mut s, mut side, knob) = playing_knob();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    s.graph()
        .replace(
            knob,
            Replacement::new(ForkByClone(Events::<0, 1>)).fade(Fade::new(Samples::ZERO)),
        )
        .expect("accepted");
    let mut out = [0.0f32];
    let mut landed = false;
    for _ in 0..8 {
        landed |= s
            .update()
            .iter()
            .any(|e| matches!(e, SessionEvent::Replaced { node } if *node == knob.erase()));
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert!(landed, "reported as replaced");
    assert_eq!(out[0], 0.0, "the incoming node renders");
}

/// **While a swap is parked, the returned key names the incoming node**, for
/// a node of the same type and of another: a write through the key reaches
/// the incoming node's values and lands with it (the value it renders is the
/// write, not its default), and the outgoing node plays on as it was.
#[test]
fn a_write_through_the_key_of_a_parked_swap_lands_with_the_incoming_node() {
    // The same type.
    let (mut s, mut side, knob) = playing_knob();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let same = s
        .graph()
        .replace(knob, Replacement::new(Params(Knob::at(3.0))))
        .unwrap();
    s.set(same.drive().to(Drive::new(7.0))).unwrap();
    let mut out = [0.0f32];
    // The executor checks its units out for the re-prepare (a silent
    // block) and resumes them; the swap waits for an update that finds the
    // graph resumed.
    assert!(s.update().is_empty());
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert!(s.update().is_empty());
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(f64::from(out[0]), Knob::BUILT_WITH, "the outgoing plays on");
    for _ in 0..8 {
        let _ = s.update();
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);
    for _ in 0..512 {
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert_eq!(out[0], 7.0, "the write, not the incoming default 3.0");

    // Another type: a write through its key, which the outgoing node's
    // table would refuse as a type mismatch.
    let (mut s, mut side, knob) = playing_knob();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let dial = s
        .graph()
        .replace(knob, Replacement::new(Params(Dial::at(3.0))))
        .unwrap();
    s.set(dial.drive().to(Drive::new(5.0)))
        .expect("the key's targets are the incoming node's");
    for _ in 0..8 {
        let _ = s.update();
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);
    for _ in 0..512 {
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert_eq!(out[0], 5.0, "the write, not the incoming default 3.0");
}

/// **While a swap is parked, `gain_reduction` answers for the incoming node**:
/// a three-band compressor replaces a two-band one, and its third band is
/// readable through the key from the call on (the outgoing node has two).
#[test]
fn the_readers_of_a_parked_swaps_key_answer_for_the_incoming_node() {
    use tutti_nodes::{MultibandBands, MultibandCompressorConfig, MultibandCompressorNode};

    let mut s = detached(1);
    let _side = s.graph().take_audio_side();
    let band = |n| {
        MultibandCompressorNode::from(MultibandCompressorConfig::new(
            tutti_buffers::BusTopology::STEREO,
            MultibandBands::of(n),
        ))
    };
    let two = s.graph().add(band(2));
    assert!(s.graph_ref().gain_reduction(&two, 2).is_none());
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let three = s
        .graph()
        .replace(two, Replacement::new(band(3)))
        .expect("accepted");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 1);
    assert!(
        s.graph_ref().gain_reduction(&three, 2).is_some(),
        "the key names the incoming node's controls"
    );
}

/// **A removal drops a parked swap and says so**: the graph re-prepares, a
/// replacement waits, the node is removed, and the next update reports
/// `ReplaceDropped` for the node, once.
#[test]
fn removing_a_node_reports_the_parked_swap_it_drops() {
    let (mut s, _side, knob) = playing_knob();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    s.graph()
        .replace(knob, Replacement::new(Params(Knob::at(3.0))))
        .unwrap();
    s.remove(knob).unwrap();
    let mut dropped = Vec::new();
    for _ in 0..4 {
        for event in s.update() {
            if let SessionEvent::ReplaceDropped { node } = event {
                dropped.push(node);
            }
        }
    }
    assert_eq!(dropped, vec![knob.erase()]);
}

/// **`remove` fades the node out, and `.unfaded()` cuts it**: a removed node
/// that feeds the root plays on through the declick after the commit that
/// carries the removal, and is gone at that commit with `.unfaded()`.
#[test]
fn an_unfaded_removal_cuts_where_the_default_fades_out() {
    for unfaded in [false, true] {
        let (mut s, mut side, knob) = playing_knob();
        let removal = s.remove(knob).unwrap();
        if unfaded {
            let _ = removal.unfaded();
        }
        assert!(s.update().is_empty());
        let mut out = [0.0f32];
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
        if unfaded {
            assert_eq!(out[0], 0.0, "cut at the block boundary");
        } else {
            assert!(
                out[0] > 0.0 && f64::from(out[0]) <= Knob::BUILT_WITH,
                "fading out: {}",
                out[0]
            );
        }
    }
}

/// **On a poisoned graph a replace is refused**: nothing is parked (no
/// update will ever take it).
#[test]
fn a_replace_on_a_poisoned_graph_is_refused() {
    let mut s = detached(1);
    let knob = s.graph().add(Params(Knob::new()));
    s.graph().add(Unforkable(Grenade));
    let _ = s.update();
    // Both halves run here (the executor is local); the second panics in
    // the grenade's `prepare` and poisons the editor.
    s.graph().set_sample_rate(SampleRate::SR_96K).unwrap();
    let refused = s
        .graph()
        .replace(knob, Replacement::new(Params(Knob::at(3.0))));
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        s.graph_ref().testing().waiting_replacements(),
        0,
        "nothing waits"
    );
    assert!(s.update().is_empty());
}

// ---- params and forks -----------------------------------------------------------

/// **Every param write path the session has reaches a fork of a node**
/// (the constraint export by `Editor::fork` rests on): `Session::set`
/// through the node's addressed `ParamSet`, and a write through the
/// session's `Values` handle. A fork starts from the values its cells hold.
/// (The modulated paths are `mod_lanes.rs`'.)
#[test]
fn every_param_write_path_reaches_a_fork() {
    let mut s = detached(2);
    let k0 = s.graph().add(Params(Knob::new()));
    let k1 = s.graph().add(Params(Knob::new()));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(k0, 0)])
        .unwrap()
        .unfaded();
    s.graph()
        .set_sources(Root::output(1), [OutPort::new(k1, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();

    s.set(k0.drive().to(Drive::new(5.0))).unwrap();
    s.values().set(k1.drive().to(Drive::new(4.0))).unwrap();
    let mut live = [0.0f32; 2];
    s.render_frame(InterleavedMut::one_frame(&mut live).unwrap());
    assert_eq!(live, [5.0, 4.0], "each write reaches the live node");
    let mut fork = s
        .graph_ref()
        .testing()
        .fork_live()
        .expect("every knob forks");
    let mut forked = [0.0f32; 2];
    fork.tick(InterleavedMut::one_frame(&mut forked).unwrap());
    assert_eq!(forked, [5.0, 4.0], "and the fork");
}

// ---- event feeds ----------------------------------------------------------------

/// **`move_feeds` moves every feeder's sources** from one sink to another:
/// a host whose sink was re-bound to a new node keeps what each feeder fed
/// it, and the old sink is fed by nothing. Each feeder keeps its own entry,
/// and the graph's event edges are their union.
#[test]
fn move_feeds_moves_every_feeders_sources() {
    let mut s = detached(1);
    let g = s.graph();
    let old = g.add(ForkByClone(Events::<1, 0>));
    let new = g.add(ForkByClone(Events::<1, 0>));
    let clip = g.add(ForkByClone(Events::<0, 1>));
    let route = g.add(ForkByClone(Events::<0, 1>));
    g.set_feed(old, Feeder::Sequencer, vec![clip.events_out()])
        .unwrap();
    g.set_feed(old, Feeder::Routes, vec![route.events_out()])
        .unwrap();
    let both = {
        let mut v = vec![clip.events_out(), route.events_out()];
        v.sort();
        v
    };
    assert_eq!(
        g.feeds(old).unwrap(),
        both,
        "each feeder kept its own entry"
    );
    let mut edges = g.sources(EventInPort::new(old, 0)).unwrap();
    edges.sort();
    assert_eq!(edges, both, "the union is the sink's event edges");

    g.move_feeds(old, new);
    assert_eq!(g.feed(new, Feeder::Sequencer).unwrap(), [clip.events_out()]);
    assert_eq!(g.feed(new, Feeder::Routes).unwrap(), [route.events_out()]);
    assert!(
        g.feeds(old).unwrap().is_empty(),
        "the old sink is fed by nothing"
    );
    let mut edges = g.sources(EventInPort::new(new, 0)).unwrap();
    edges.sort();
    assert_eq!(edges, both, "the new sink's edges are the union");
    assert!(
        g.sources(EventInPort::new(old, 0)).unwrap().is_empty(),
        "and the old's are gone"
    );
    assert_eq!(s.update().len(), 0);
    assert!(!s.graph_ref().has_uncommitted_edits(), "it commits");
}

// ---- rendering --------------------------------------------------------------------

/// Mean period, in frames, of the rising zero crossings in `x`
/// (interpolated between the two frames that straddle each).
fn period(x: &[f32]) -> f64 {
    let crossings: Vec<f64> = (1..x.len())
        .filter(|&i| x[i - 1] < 0.0 && x[i] >= 0.0)
        .map(|i| (i - 1) as f64 + f64::from(x[i - 1] / (x[i - 1] - x[i])))
        .collect();
    assert!(crossings.len() > 10, "a tone to measure");
    (crossings[crossings.len() - 1] - crossings[0]) / (crossings.len() - 1) as f64
}

/// `frames` frames of `s`, `layout` wide, in `block`-frame blocks.
fn pull(s: &mut Session, layout: ChannelLayout, frames: usize, block: usize) -> Vec<f32> {
    let width = usize::from(layout.count());
    let mut out = Vec::with_capacity(frames * width);
    let mut buf = vec![0.0f32; block * width];
    while out.len() < frames * width {
        s.render(&mut InterleavedMut::new(&mut buf, layout).unwrap());
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * width);
    out
}

/// **An offline session with a 1 kHz sine renders its analytic RMS**:
/// `1/√2` over a whole number of cycles (one second at 48 kHz is 1 000 of
/// them), at 48 frames a cycle (the graph runs at the rate it was built
/// at).
#[test]
fn an_offline_session_renders_a_sines_analytic_rms() {
    let mut s = Session::builder()
        .sample_rate(RATE)
        .outputs(ChannelLayout::MONO)
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("builds");
    let osc = s.graph().add(ForkByClone(Osc::sine(Hz::new(1_000.0))));
    for c in 0..s.graph_ref().outputs() {
        s.graph()
            .connect(osc.out(0), Root::output(c))
            .unwrap()
            .unfaded();
    }
    let _ = s.update();
    let out = pull(&mut s, ChannelLayout::MONO, 48_000, 512);
    let rms = tutti_fixtures::signal::rms_f64(&out);
    assert!(
        (rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-4,
        "RMS {rms}, a unit sine's is 1/√2"
    );
    assert!(
        (period(&out) - 48.0).abs() < 1e-3,
        "{} frames a cycle",
        period(&out)
    );
}

/// **A latent node on one branch is compensated on the other, to the
/// sample**: a sine straight to output 0 and through a 37-frame delay that
/// declares its 37 frames to output 1. The dry branch is delayed by exactly
/// 37 frames, so the two outputs are the same samples, bit for bit, both
/// silent for the first 37 frames (the sine's first sample is `sin(0)`, so
/// the first sound is frame 38); the session publishes the figures before
/// the first block: a total of 37, and a pre-roll of 37 on the dry channel
/// (a source outside the graph seeks by it) and 0 on the latent one.
#[test]
fn a_latent_branch_is_compensated_on_the_other_to_the_sample() {
    const N: usize = 37;
    let mut s = Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(256)))
        .build()
        .expect("builds");
    let osc = s.graph().add(ForkByClone(Osc::sine(Hz::new(440.0))));
    let late = s.graph().add(ForkByClone(Late::<N>::new()));
    s.graph()
        .set_sources(InPort::new(late, 0), [OutPort::new(osc, 0)])
        .unwrap()
        .unfaded();
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(osc, 0)])
        .unwrap()
        .unfaded();
    s.graph()
        .set_sources(Root::output(1), [OutPort::new(late, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert_eq!(s.latency(), Samples::new(N), "the total is published");
    assert_eq!(
        s.compensation().to_vec().as_slice(),
        [Samples::new(N), Samples::new(0)],
        "the dry channel pre-rolls by the latent one's latency"
    );

    let out = pull(&mut s, ChannelLayout::STEREO, 4_096, 256);
    let (dry, wet): (Vec<f32>, Vec<f32>) =
        out.as_chunks::<2>().0.iter().map(|f| (f[0], f[1])).unzip();
    let first = |x: &[f32]| x.iter().position(|&v| v != 0.0);
    assert_eq!(first(&wet), Some(N + 1), "the latent branch starts late");
    assert_eq!(first(&dry), Some(N + 1), "the dry branch waits for it");
    assert!(
        dry.iter()
            .zip(&wet)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "the same samples on both, to the bit"
    );
}

/// **A removal, a commit and an update are any thread's**: no path of `Session` frees a plugin,
/// so none asserts the main thread.
/// What a plugin's removal retires waits in the graveyard for the main thread's update
/// (`src/plugin/fake_hosting/editor.rs`), and a format hands its in-process release to
/// the main thread; the session's own paths run wherever the session is.
///
/// The session is driven from another thread after the process's main thread has
/// been claimed here, and goes back to be dropped.
#[cfg(feature = "plugin")]
#[test]
fn a_removal_a_commit_and_an_update_run_on_any_thread() {
    // The test thread is the process's main thread; the work below is not on it.
    let _main = tutti_fixtures::main_thread();
    let mut s = detached(1);
    let dc = s.graph().add(ForkByClone(Const::mono(0.5)));
    let _ = s.update();

    let (s, driven) = std::thread::spawn(move || {
        let mut s = s;
        let removed = s.remove(dc).map(|_| ());
        s.commit();
        let _ = s.update();
        (s, removed)
    })
    .join()
    .expect("none of removing, committing and updating off the main thread panics");
    assert_eq!(driven, Ok(()), "the node was in the graph");
    drop(s);
}

/// A source of zeros whose controls publish to it through a `Publisher`
/// cell, as a clip or a voice placement does: the node lends once per
/// block, and its `SessionNode` impl hands the graph a reclaimer over its
/// controls.
struct Published {
    node: PublishedNode,
    controls: PublishedControls,
}

/// [`Published`]'s unit: the reader it lends through.
struct PublishedNode {
    reader: tutti_rt::Reader<u32>,
}

/// [`Published`]'s controls: its cell, and a token only the controls hold
/// (the unit holds none), so a test counts the clones alive (the test's,
/// the graph's reclaimer's and the graph's own, kept for the node's life).
#[derive(Clone)]
struct PublishedControls {
    cell: tutti_rt::Publisher<u32>,
    token: Arc<()>,
}

/// The cell's `reclaim`, as an engine node's controls have one.
impl tutti_rt::Reclaim for PublishedControls {
    fn reclaim(&self) -> usize {
        self.cell.reclaim()
    }
}

impl Published {
    /// The node, and the test's own clone of its controls.
    fn new() -> (Self, PublishedControls) {
        let cell = tutti_rt::Publisher::new(0);
        let controls = PublishedControls {
            cell,
            token: Arc::new(()),
        };
        let node = Self {
            node: PublishedNode {
                reader: controls.cell.reader(),
            },
            controls: controls.clone(),
        };
        (node, controls)
    }
}

impl Node for PublishedNode {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let _block = self.reader.lend();
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl IntoNode for Published {
    type Controls = PublishedControls;
    fn into_parts(self) -> NodeParts<PublishedControls> {
        let NodeParts { node, fork, .. } = IntoNode::into_parts(Unforkable(self.node));
        NodeParts {
            node,
            controls: self.controls,
            fork,
        }
    }
}

impl SessionNode for Published {
    fn reclaimer(controls: &PublishedControls) -> tutti_session::Reclaims {
        tutti_session::Reclaims::of(controls)
    }
}

/// Publishes into `controls`' cell while a borrow on it (a block in
/// progress) is held, then lets the borrow go: what is left is one retired
/// value nothing pins, which only a scan frees.
fn replace_under_a_held_borrow(controls: &PublishedControls) {
    let mut block = controls.cell.reader();
    let _held = block.lend();
    controls.cell.publish(Arc::new(1));
}

/// **The update sweeps each node's reclaimer while it is in the graph**:
/// a value its cell retired while a block held it is freed by the next
/// update, for a node `add`ed and for the node a replace put in its
/// place; the reclaimer goes with the node (replaced by the incoming one's,
/// dropped by a raw replace and by the removal), so a node out of the graph
/// is not swept. The outgoing node's controls outlive its reclaimer until
/// the update that collects its retired unit.
#[test]
fn the_update_sweeps_each_nodes_reclaimer_while_it_is_in_the_graph() {
    let mut s = detached(1);
    let (published, first) = Published::new();
    let node = s.graph().add(published);
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert_eq!(
        Arc::strong_count(&first.token),
        3,
        "the graph keeps two: the reclaimer's and the node's controls"
    );

    replace_under_a_held_borrow(&first);
    assert_eq!(first.cell.retired_len(), 1, "retired, not yet scanned");
    let _ = s.update();
    assert_eq!(first.cell.retired_len(), 0, "the update freed it");

    let (incoming, second) = Published::new();
    s.graph()
        .replace(
            node,
            Replacement::new(incoming).fade(Fade::new(Samples::new(0))),
        )
        .expect("the node is in the graph");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);
    assert_eq!(
        Arc::strong_count(&first.token),
        2,
        "the outgoing node's reclaimer went with it; its controls wait for its unit"
    );
    assert_eq!(
        Arc::strong_count(&second.token),
        3,
        "the incoming one's kept"
    );
    let _ = s.update();
    assert_eq!(
        Arc::strong_count(&first.token),
        1,
        "the update collected the outgoing unit and dropped its controls"
    );
    replace_under_a_held_borrow(&second);
    let _ = s.update();
    assert_eq!(second.cell.retired_len(), 0, "the incoming node is swept");

    // A replace by a node with no reclaimer (a plain clone-forked one)
    // takes the outgoing node's with it.
    assert!(
        s.graph()
            .replace(
                node,
                Replacement::new(ForkByClone(Const::mono(0.0))).fade(Fade::new(Samples::new(0)))
            )
            .is_ok()
    );
    assert_eq!(
        Arc::strong_count(&second.token),
        2,
        "the replaced node's reclaimer went with it"
    );
    let _ = s.update();
    assert_eq!(
        Arc::strong_count(&second.token),
        1,
        "and its controls with its collected unit"
    );

    let (published, third) = Published::new();
    let other = s.graph().add(published);
    let _ = s.update();
    s.remove(other).unwrap();
    assert_eq!(
        Arc::strong_count(&third.token),
        2,
        "the removal dropped its reclaimer; its controls wait for its unit"
    );
    replace_under_a_held_borrow(&third);
    let _ = s.update();
    assert_eq!(
        Arc::strong_count(&third.token),
        1,
        "the update collected the removed unit and dropped its controls"
    );
    assert_eq!(third.cell.retired_len(), 1, "a removed node is not swept");
}

/// **A removed node's controls live until its output has faded out**: the
/// node renders through its declicked removal, so what its controls hold (a
/// voice's stream) is dropped only by the update that collects its unit,
/// while `handle` and every key read answer `None` from the removal on.
#[test]
fn a_removed_nodes_controls_live_until_its_fade_out_ends() {
    let mut s = Session::builder()
        .sample_rate(RATE)
        .build()
        .expect("builds");
    let (published, controls) = Published::new();
    let node = s.graph().add(published);
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    let _ = pull(&mut s, ChannelLayout::STEREO, 64, 64);
    s.remove(node).unwrap();
    let _ = s.update();
    assert_eq!(
        Arc::strong_count(&controls.token),
        2,
        "the node still renders its fade-out, so the graph holds its controls"
    );
    // The declick spans 256 frames; render past it, then let the updates
    // drop the node and collect its unit.
    let _ = pull(&mut s, ChannelLayout::STEREO, 1_024, 64);
    for _ in 0..3 {
        let _ = s.update();
        let _ = pull(&mut s, ChannelLayout::STEREO, 64, 64);
    }
    assert_eq!(
        Arc::strong_count(&controls.token),
        1,
        "the collected unit took the controls with it"
    );
}

/// **Two parked replaces of one node, the first refused when it lands, leave
/// the second's controls under the key**: while a re-prepare is between its
/// commits a replace over another shape (refused at landing) and a replace
/// that cuts both wait. The refusal passes the outgoing controls to the
/// second, which lands: a write through its key reaches it, and the outgoing
/// node's controls are held until its unit has left, then dropped.
#[test]
fn a_refused_parked_replace_leaves_the_next_ones_controls_under_the_key() {
    let mut s = detached(1);
    let mut side = s.graph().take_audio_side();
    let (published, outgoing) = Published::new();
    let node = s.graph().add(published);
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap()
        .unfaded();
    assert!(s.update().is_empty());
    let mut out = [0.0f32];
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());

    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    s.graph()
        .replace(node, Replacement::new(ForkByClone(Events::<0, 1>)))
        .expect("accepted: the shape is checked when it lands");
    let knob = s
        .graph()
        .replace(
            node,
            Replacement::new(Params(Knob::at(3.0))).fade(Fade::new(Samples::ZERO)),
        )
        .expect("accepted");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 2);

    let mut events = Vec::new();
    for _ in 0..8 {
        events.extend(s.update());
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ReplaceFailed { .. })),
        "the first was refused: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Replaced { .. })),
        "the second landed: {events:?}"
    );

    s.set(knob.drive().to(Drive::new(7.0)))
        .expect("the key addresses the node that landed");
    for _ in 0..8 {
        let _ = s.update();
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
    }
    assert_eq!(out[0], 7.0, "the write reached the second node");
    assert_eq!(
        Arc::strong_count(&outgoing.token),
        1,
        "the outgoing controls were dropped, once its unit had left"
    );
}

/// A pool's voice added and then removed, rendered through, so the pool's
/// audio side parks the removed slot in its retire ring.
#[cfg(feature = "sampler")]
fn park_a_retired_slot(s: &mut DetachedSession, handle: &tutti_sampler::VoicePoolHandle<f32>) {
    use tutti_sampler::{MemorySource, Playback, SlotId, VoiceWindow};
    let source = MemorySource::placed(
        Arc::new(tutti_buffers::Wave::new(1, SampleRate::SR_48K)),
        VoiceWindow::from(tutti_units::Beat::new(0.0)),
    )
    .expect("a placeable wave");
    handle
        .add(SlotId(1), source, Playback::default())
        .expect("queued");
    let mut out = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    handle.remove(SlotId(1)).expect("queued");
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
}

/// **The update frees the slots a pool the graph holds retired**: `Graph::add`
/// of a `VoicePool` keeps `VoicePoolHandle`'s reclaimer, and
/// `Phase::Reclaim` (the update) drains the pool's retire ring on the control
/// side. A pool inserted without one (`Unforkable`) keeps its parked slot, so
/// the sweep, not the render, is what freed the first.
#[cfg(feature = "sampler")]
#[test]
fn the_update_frees_the_slots_a_graph_pool_retired() {
    let mut s = detached(1);
    let pool = s.graph().add(tutti_sampler::VoicePool::new());
    let handle = s
        .graph_ref()
        .handle(&pool)
        .expect("the pool is in the graph")
        .clone();
    let (bare, bare_handle) = tutti_sampler::VoicePool::new().with_handle();
    let bare = s.graph().add(tutti_graph::Unforkable(bare));
    s.graph()
        .set_sources(
            Root::output(0),
            [OutPort::new(pool, 0), OutPort::new(bare, 0)],
        )
        .unwrap()
        .unfaded();
    let _ = s.update();
    park_a_retired_slot(&mut s, &handle);
    park_a_retired_slot(&mut s, &bare_handle);

    let _ = s.update();
    assert_eq!(
        tutti_rt::Reclaim::reclaim(&handle),
        0,
        "nothing left for a second sweep"
    );
    assert_eq!(
        handle.collect_retired(),
        0,
        "the update freed the graph pool's retired slot"
    );
    assert_eq!(
        bare_handle.collect_retired(),
        1,
        "a pool with no reclaimer keeps its parked slot"
    );
}

/// **A handle is `None` once its node is removed**: the key names a node the
/// graph no longer holds, so `Graph::handle` finds no controls for it.
#[cfg(feature = "sampler")]
#[test]
fn a_handle_is_none_once_its_node_is_removed() {
    let mut s = detached(1);
    let pool = s.graph().add(tutti_sampler::VoicePool::new());
    assert!(s.graph_ref().handle(&pool).is_some(), "in the graph");
    s.remove(pool).unwrap();
    assert!(s.graph_ref().handle(&pool).is_none(), "removed");
}

/// **A streamed voice keeps sounding after the host drops its clone of the
/// pool's handle, and the graph's handle still commands it**: the pool's handle
/// holds each streaming voice's `Stream`, and the last handle dropped closes
/// it. The graph owns one for the node's life, so the file (a constant
/// 0.25) still renders at unity gain on the root's left channel (the file is
/// mono) after the streamer has stepped past the drop, and a removal queued
/// through `Graph::handle` silences it.
#[cfg(feature = "sampler")]
#[test]
fn a_streamed_voice_keeps_sounding_after_the_host_drops_its_handle() {
    use tutti_sampler::{DiskStreamer, DiskVoice, Playback, SlotId, StreamStart, VoiceWindow};
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("level.wav");
    tutti_fixtures::signal::write_wav_f32(&path, RATE, 1, &[0.25f32; 48_000]);
    let mut streamer = DiskStreamer::manual(RATE, Default::default()).expect("builds");
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stream, reader) =
        tutti_sampler::testing::wait_for_open(&mut streamer, opening).expect("the file opens");

    let mut s = Session::builder()
        .sample_rate(RATE)
        .build()
        .expect("builds");
    let pool = s.graph().add(tutti_sampler::VoicePool::new());
    let host = s
        .graph()
        .handle(&pool)
        .expect("the pool is in the graph")
        .clone();
    s.graph().connect(pool, Root).unwrap().unfaded();
    host.add_streaming(
        SlotId(1),
        DiskVoice::from_reader(reader, VoiceWindow::from(tutti_units::Beat::new(0.0))),
        stream,
        Playback::default(),
    )
    .expect("queued");
    drop(host);
    let _ = streamer.step_until_idle(1_000);
    let _ = s.update();
    s.play().expect("room");
    let mut block = [0.0f32; 1_024];
    for _ in 0..8 {
        let _ = streamer.step_until_idle(1_000);
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    }
    assert_eq!(block[1_022], 0.25, "the voice is still sounding");

    s.graph_ref()
        .handle(&pool)
        .expect("the pool is in the graph")
        .remove(SlotId(1))
        .expect("queued");
    for _ in 0..4 {
        let _ = streamer.step_until_idle(1_000);
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    }
    assert_eq!(block[1_022], 0.0, "the graph's handle removed the voice");
}

/// The widest global input a layout holds builds: the count is a
/// `ChannelLayout`, so one no layout holds cannot be written.
#[test]
fn the_widest_input_layout_builds() {
    assert!(
        Session::builder()
            .inputs(ChannelLayout::from_count(u16::MAX))
            .build_detached(RATE)
            .is_ok()
    );
}
