//! The node a refused rate mutes, in a real graph (so the executor parks it
//! when it may) over the fake plugin, whose log says which Processor ran what.

use std::sync::Arc;
use tutti_units::ParamId;

use tutti_buffers::ChannelLayout;
use tutti_fixtures::{main_thread, midi};
use tutti_graph::{
    EventCapacity, EventInletNode, EventInletWriter, Fade, GraphBuilder, NodeKey, QUEUE_CAPACITY,
    Renderer, Unforkable,
};
use tutti_plugin_types::fake::{Call, FakeLoaded, FakePlugin};
use tutti_plugin_types::{Control, Loaded, MaxBlock, PluginInvalidation, Prepare};
use tutti_rt::Mutex;
use tutti_tempo::TempoMap;
use tutti_timeline::TempoReader;
use tutti_ump::{MidiChannel, U7};
use tutti_units::{Bpm, Frame, SampleRate, Samples};

use super::instance::Instance;
use super::rebind::{Rebinder, Settlement};
use crate::client_over;

const A: SampleRate = SampleRate::SR_48K;
const B: SampleRate = SampleRate::SR_96K;
const C: SampleRate = SampleRate::SR_44K1;
const BLOCK: usize = 64;
/// A bound on a loop that waits for an event, never a measure of speed.
const LIMIT: usize = 64;

struct Rig {
    renderer: Renderer,
    inner: Arc<Mutex<Instance<FakeLoaded>>>,
    rebinder: Rebinder<FakeLoaded>,
    fake: FakePlugin,
    /// The host's parameter writes to the plugin.
    post: super::host_edits::EditPost,
    key: NodeKey,
    events: EventInletWriter,
    frame: u64,
}

fn tempo() -> TempoReader {
    TempoReader::fixed(Arc::new(
        TempoMap::constant(Bpm::of(120.0)).expect("a tempo"),
    ))
}

impl Rig {
    /// An effect over the fake at rate `A` with an event inlet feeding it. The
    /// fake sleeps in a block that gives it nothing, so the node is `Idle`
    /// and the executor parks it.
    fn new() -> Self {
        let (fake, loaded) = FakePlugin::new();
        fake.ring_out(1);
        fake.declare(&[(ParamId::new(1), false)]);
        let main = main_thread();
        let wiring = loaded.wiring(main);
        let descriptor = loaded.info().clone();
        let prepare = Prepare::new(A, MaxBlock::new(Samples::new(BLOCK)).expect("a block"));
        let Ok((processor, controller)) = loaded.activate(main, prepare) else {
            panic!("the fake activates");
        };
        let mut client = client_over::<FakeLoaded, f32>(
            processor,
            controller,
            prepare,
            descriptor,
            &wiring,
            [ParamId::new(1)],
            0,
            None,
        )
        .expect("the fake is narrow");
        let (post, port) = super::host_edits::host_edits([ParamId::new(1)]);
        client.edits = Some(port);
        let (inner, rebinder) = (Arc::clone(&client.inner), client.rebinder(tempo()));
        let layout = ChannelLayout::from_count(2);
        let mut graph = GraphBuilder::new(ChannelLayout::EMPTY, layout);
        let key = graph.add(Unforkable(client.bind(tempo())));
        let (inlet, events) = EventInletNode::pair(EventCapacity::of(64));
        let from = graph.add(inlet);
        graph.connect_events(from, 0, key, 0);
        graph.pipe_output(key);
        let renderer = graph
            .renderer(tutti_graph::Prepare::new(
                A,
                tutti_buffers::MaxBlock::of(Samples::new(BLOCK)),
            ))
            .expect("a one-plugin graph compiles");
        fake.clear_calls();
        Self {
            renderer,
            inner,
            rebinder,
            fake,
            post,
            key,
            events,
            frame: 0,
        }
    }

    /// One block, nothing on the audio inputs; the first output channel.
    fn block(&mut self) -> Vec<f32> {
        self.frame += BLOCK as u64;
        self.renderer.render(BLOCK).swap_remove(0)
    }

    fn note(&mut self) {
        let note = midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100));
        self.events
            .push(Frame::new(self.frame), note)
            .expect("room");
    }

    fn pump(&self) -> Vec<PluginInvalidation> {
        self.inner.lock().pump(main_thread()).reports
    }

    fn processed(&self) -> usize {
        self.fake
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Process(_)))
            .count()
    }

    /// The plugin is asked for `rate` the way the pump path asks.
    fn ask(&self, rate: SampleRate) {
        let mut instance = self.inner.lock();
        instance.want(|w| w.with_rate(rate));
        instance.rate_asked();
        instance
            .reconcile(Some(main_thread()), None)
            .expect("the change starts");
    }

    /// Blocks and pumps, a block between every control step, until `done`;
    /// the reports so far.
    fn until(
        &mut self,
        what: &str,
        done: impl Fn(&Rig, &[PluginInvalidation]) -> bool,
    ) -> Vec<PluginInvalidation> {
        let mut seen = Vec::new();
        for _ in 0..LIMIT {
            self.block();
            seen.extend(self.pump());
            self.block();
            if done(self, &seen) {
                return seen;
            }
        }
        panic!("no {what}: {seen:?}");
    }

    /// Until the report `want` accepts arrives.
    fn until_reported(
        &mut self,
        what: &str,
        want: fn(&PluginInvalidation) -> bool,
    ) -> Vec<PluginInvalidation> {
        self.until(what, |_, seen| seen.iter().any(want))
    }

    /// Blocks until the executor has parked the node: the fake is not called.
    fn rest(&mut self) {
        for _ in 0..8 {
            self.block();
        }
        let before = self.processed();
        for _ in 0..8 {
            self.block();
        }
        assert_eq!(self.processed(), before, "the executor parked the node");
    }

    fn refuse_b(&mut self) {
        self.fake.refuse_rate(Some(B));
        self.ask(B);
        self.until_reported("refused rate", |r| {
            matches!(r, PluginInvalidation::RateRefused(_))
        });
    }
}

fn restored(r: &PluginInvalidation) -> bool {
    matches!(r, PluginInvalidation::RateRestored)
}

/// **A recall reaches a node the executor parked, and a node muted by a refused
/// rate is never parked, so a later rate change completes.** The fake sleeps,
/// so the executor stops calling the node (it has an event input, which parks
/// only on `Status::Idle`); a rate change recalls the Processor, the node is
/// woken to hand it back, the refused rate mutes it, and the node is still
/// called (`Silent`, not `Idle`), so the accepted rate that follows recalls and
/// takes the new Processor, clears the mute and is reported once.
#[test]
fn a_recall_reaches_a_parked_node_and_a_muted_node_is_never_parked() {
    let mut rig = Rig::new();
    rig.rest();
    rig.refuse_b();
    assert!(rig.inner.lock().silenced().holds(), "muted");
    assert!(rig.block().iter().all(|&s| s == 0.0), "muted: silence");

    rig.fake.refuse_rate(None);
    rig.ask(C);
    let reports = rig.until_reported("restored rate", restored);
    assert_eq!(
        reports.iter().filter(|r| restored(r)).count(),
        1,
        "{reports:?}"
    );
    assert!(!rig.inner.lock().silenced().holds(), "unmuted");
    assert!(
        rig.fake
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Activate(rate, _) if *rate == C))
    );
}

/// **The recall that moves the Processor to a successor reaches the node that
/// holds it while the successor is only staged.** The successor's wake is bound
/// when the editor stages it; the executor still runs, and parks, the
/// predecessor until a commit lands. The commit here is refused
/// (`Backpressure`: the executor has not run since the queue was filled), so
/// the `Relend` recall must call the predecessor, not the successor.
#[test]
fn a_recall_reaches_the_node_that_holds_the_processor_while_its_successor_is_staged() {
    let mut rig = Rig::new();
    rig.rest();
    let (successor, pending) = rig.rebinder.prepare().expect("a node");
    let editor = rig.renderer.editor_mut();
    for n in 0..QUEUE_CAPACITY {
        let (inlet, _writer) = EventInletNode::pair(EventCapacity::of(1));
        editor.insert(NodeKey(100 + n as u64), "filler", inlet);
        editor.commit().expect("the queue has room");
    }
    let fade = Fade::new(Samples::new(0));
    editor
        .replace(rig.key, Unforkable(successor), fade)
        .expect("staged");
    assert!(matches!(
        editor.commit(),
        Err(tutti_graph::CommitError::Backpressure)
    ));
    pending.commit(main_thread());
    let rebinder = rig.rebinder.clone();
    rig.until("the Processor moved", |_, _| {
        rebinder.settled() == Settlement::Moved
    });
}

/// **The plugin's own request to be processed raises the wake of the node that
/// holds the Processor**, whichever node that is: the node the executor runs
/// while its successor is staged, and the successor once the Processor moved.
/// The fake sleeps, so the executor parks the node that holds it; a request,
/// answered at the instance's next pump, is what calls it again.
#[test]
fn the_plugins_process_request_wakes_the_node_that_holds_the_processor() {
    let mut rig = Rig::new();
    rig.rest();
    let (successor, pending) = rig.rebinder.prepare().expect("a node");
    let editor = rig.renderer.editor_mut();
    for n in 0..QUEUE_CAPACITY {
        let (inlet, _writer) = EventInletNode::pair(EventCapacity::of(1));
        editor.insert(NodeKey(100 + n as u64), "filler", inlet);
        editor.commit().expect("the queue has room");
    }
    editor
        .replace(rig.key, Unforkable(successor), Fade::new(Samples::new(0)))
        .expect("staged");
    assert!(matches!(
        editor.commit(),
        Err(tutti_graph::CommitError::Backpressure)
    ));

    let before = rig.processed();
    rig.fake.request_process();
    let _ = rig.pump();
    rig.block();
    assert!(
        rig.processed() > before,
        "the request called the node that holds the Processor while its successor is staged"
    );

    let sent = (0..LIMIT).any(|_| {
        rig.block();
        rig.renderer.editor_mut().commit().is_ok()
    });
    assert!(sent, "the commit that carries the successor is sent");
    pending.commit(main_thread());
    let rebinder = rig.rebinder.clone();
    rig.until("the Processor moved", |_, _| {
        rebinder.settled() == Settlement::Moved
    });
    rig.rest();
    let before = rig.processed();
    rig.fake.request_process();
    let _ = rig.pump();
    rig.block();
    assert!(
        rig.processed() > before,
        "the request called the successor, which holds the Processor now"
    );
}

/// **No Processor activated at the rolled-back rate processes a block between
/// the refusal and the first block at the next accepted rate**, with a block
/// run after every control step: the mute is cleared only once the plugin is
/// activated at that rate and its Processor lent.
#[test]
fn the_audio_thread_never_renders_with_the_rolled_back_processor() {
    let mut rig = Rig::new();
    for _ in 0..4 {
        rig.note();
        rig.block();
    }
    rig.refuse_b();
    let rolled_back = rig
        .fake
        .calls()
        .iter()
        .rposition(|c| matches!(c, Call::Activate(rate, _) if *rate == A))
        .expect("the plugin was activated again at the accepted rate");
    // Pumped and rendered with the refusal standing.
    for _ in 0..3 {
        rig.note();
        rig.block();
        let _ = rig.pump();
        rig.block();
    }
    rig.fake.refuse_rate(None);
    rig.ask(C);
    rig.until_reported("restored rate", restored);
    let calls = rig.fake.calls();
    let at_c = calls
        .iter()
        .position(|c| matches!(c, Call::Activate(rate, _) if *rate == C))
        .expect("the plugin was activated at the accepted rate");
    assert!(
        calls[rolled_back..at_c]
            .iter()
            .all(|c| !matches!(c, Call::Process(_))),
        "a block ran on the Processor activated at the rolled-back rate: {:?}",
        &calls[rolled_back..at_c]
    );
    rig.block();
    assert!(
        rig.fake.calls()[at_c..]
            .iter()
            .any(|c| matches!(c, Call::Process(_))),
        "the new Processor renders once the mute is cleared"
    );
}

/// **The MIDI a muted node is given is counted lost**, as the MIDI of a block
/// the plugin did not process is.
#[test]
fn a_muted_node_counts_the_midi_it_is_given_as_lost() {
    let mut rig = Rig::new();
    rig.refuse_b();
    let key = rig.key;
    let _ = rig.renderer.editor_mut().take_event_losses().count();
    for _ in 0..3 {
        rig.note();
    }
    rig.block();
    let lost: u32 = rig
        .renderer
        .editor_mut()
        .take_event_losses()
        .filter(|l| l.node == key)
        .map(|l| l.count)
        .sum();
    assert_eq!(lost, 3);
}

/// **A host parameter write wakes a node the executor parked.** The fake sleeps,
/// so the node is parked after a block that gave it nothing; a write is not an
/// input of its ports, and the block after it still calls the plugin.
#[test]
fn a_host_write_wakes_a_parked_node() {
    let mut rig = Rig::new();
    rig.rest();
    let before = rig.processed();
    rig.block();
    assert_eq!(rig.processed(), before, "the parked node is not called");
    rig.post
        .write(ParamId::new(1), tutti_units::Normalized::of(0.5))
        .expect("a declared parameter");
    rig.block();
    assert!(rig.processed() > before, "the write called the parked node");
}
