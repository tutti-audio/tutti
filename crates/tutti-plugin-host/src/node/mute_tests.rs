//! The node a refused rate mutes, in a real graph (so the executor parks it
//! when it may) over the fake plugin, whose log says which Processor ran what.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_fixtures::{main_thread, midi};
use tutti_graph::{
    EventInletNode, EventInletWriter, GraphBuilder, NodeKey, Prepare as GraphPrepare, Renderer,
    Unforkable,
};
use tutti_plugin_types::fake::{Call, FakeLoaded, FakePlugin};
use tutti_plugin_types::{Control, Loaded, MaxBlock, PluginInvalidation, Prepare};
use tutti_rt::Mutex;
use tutti_tempo::TempoMap;
use tutti_timeline::TempoReader;
use tutti_ump::{MidiChannel, U7};
use tutti_units::{Bpm, Frame, SampleRate, Samples};

use super::instance::Instance;
use crate::client_over;

const A: SampleRate = SampleRate::SR_48K;
const B: SampleRate = SampleRate::SR_96K;
const C: SampleRate = SampleRate::SR_44K1;
const BLOCK: usize = 64;
/// More blocks than any exchange here needs: a bound on a loop that waits for
/// an event, never a measure of speed.
const BLOCKS_LIMIT: usize = 64;

struct Rig {
    renderer: Renderer,
    inner: Arc<Mutex<Instance<FakeLoaded>>>,
    fake: FakePlugin,
    key: NodeKey,
    events: EventInletWriter,
    frame: u64,
}

impl Rig {
    /// An effect over the fake at rate `A`, an event inlet feeding its event
    /// input, silent input. The fake sleeps in a block that gives it nothing,
    /// so the node is `Status::Idle` and the executor parks it.
    fn new() -> Self {
        let (fake, loaded) = FakePlugin::new();
        fake.ring_out(1);
        let main = main_thread();
        let wiring = loaded.wiring(main);
        let descriptor = loaded.info().clone();
        let prepare = Prepare::new(A, MaxBlock::new(Samples::new(BLOCK)).expect("a block"));
        let Ok((processor, controller)) = loaded.activate(main, prepare) else {
            panic!("the fake activates");
        };
        let client = client_over::<FakeLoaded, f32>(
            processor,
            controller,
            prepare,
            descriptor,
            &wiring,
            [],
            0,
            None,
        )
        .expect("the fake is narrow");
        let inner = Arc::clone(&client.inner);
        let tempo = TempoReader::fixed(Arc::new(
            TempoMap::constant(Bpm::of(120.0)).expect("a tempo"),
        ));
        let layout = ChannelLayout::from_count(2);
        let mut graph = GraphBuilder::new(ChannelLayout::EMPTY, layout);
        let key = graph.add(Unforkable(client.bind(tempo)));
        let (inlet, events) = EventInletNode::pair(tutti_graph::EventCapacity::of(64));
        let from = graph.add(inlet);
        graph.connect_events(from, 0, key, 0);
        graph.pipe_output(key);
        let renderer = graph
            .renderer(GraphPrepare::new(
                A,
                tutti_buffers::MaxBlock::of(Samples::new(BLOCK)),
            ))
            .expect("a one-plugin graph compiles");
        fake.clear_calls();
        Self {
            renderer,
            inner,
            fake,
            key,
            events,
            frame: 0,
        }
    }

    /// One block, nothing on the audio inputs; the first output channel.
    fn block(&mut self) -> Vec<f32> {
        let out = self.renderer.render(BLOCK);
        self.frame += BLOCK as u64;
        out.into_iter().next().expect("two outputs")
    }

    /// The main thread's pump: what the plugin's control side reports.
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

    /// Blocks and pumps, a block between every control step, until a report
    /// `want` accepts arrives; the reports so far.
    fn until(
        &mut self,
        what: &str,
        want: impl Fn(&PluginInvalidation) -> bool,
    ) -> Vec<PluginInvalidation> {
        let mut seen = Vec::new();
        for _ in 0..BLOCKS_LIMIT {
            self.block();
            seen.extend(self.pump());
            self.block();
            if seen.iter().any(&want) {
                return seen;
            }
        }
        panic!("no {what} was reported: {seen:?}");
    }

    /// Runs blocks until the node has come to rest: the fake is no longer called.
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

    fn refuse_b(&mut self) -> Vec<PluginInvalidation> {
        self.fake.refuse_rate(Some(B));
        self.inner
            .lock()
            .set_sample_rate(main_thread(), B, None)
            .expect("the change starts");
        self.until("refused rate", |r| {
            matches!(r, PluginInvalidation::RateRefused(_))
        })
    }
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

    let refused = rig.refuse_b();
    assert!(
        matches!(
            refused.as_slice(),
            [PluginInvalidation::RateRefused(r)] if r.rate == B
        ),
        "{refused:?}"
    );
    assert!(rig.inner.lock().silenced().holds(), "muted");
    assert!(
        rig.block().iter().all(|&s| s == 0.0),
        "muted: silence, not the plugin at the wrong rate"
    );

    rig.fake.refuse_rate(None);
    rig.inner
        .lock()
        .set_sample_rate(main_thread(), C, None)
        .expect("the change starts");
    let reports = rig.until("restored rate", |r| {
        matches!(r, PluginInvalidation::RateRestored)
    });
    assert_eq!(
        reports
            .iter()
            .filter(|r| matches!(r, PluginInvalidation::RateRestored))
            .count(),
        1,
        "{reports:?}"
    );
    assert!(!rig.inner.lock().silenced().holds(), "unmuted");
    assert!(
        rig.fake
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Activate(rate, _) if *rate == C)),
        "the plugin was activated at the accepted rate"
    );
    let _ = rig.key;
}

/// **No Processor activated at the rolled-back rate processes a block between
/// the refusal and the first block at the next accepted rate**, with a block
/// run after every control step: the mute is cleared only once the plugin is
/// activated at that rate and its Processor lent.
#[test]
fn the_audio_thread_never_renders_with_the_rolled_back_processor() {
    let mut rig = Rig::new();
    // Sounding, so the node is not parked and every block is a candidate.
    let trigger = midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100));
    for _ in 0..4 {
        rig.events
            .push(Frame::new(rig.frame), trigger)
            .expect("room");
        rig.block();
    }
    rig.refuse_b();
    let calls = rig.fake.calls();
    let rolled_back = calls
        .iter()
        .rposition(|c| matches!(c, Call::Activate(rate, _) if *rate == A))
        .expect("the plugin was activated again at the accepted rate");

    // Pumped and rendered with the refusal standing.
    for _ in 0..3 {
        rig.events
            .push(Frame::new(rig.frame), trigger)
            .expect("room");
        rig.block();
        let _ = rig.pump();
        rig.block();
    }
    rig.fake.refuse_rate(None);
    rig.inner
        .lock()
        .set_sample_rate(main_thread(), C, None)
        .expect("the change starts");
    for _ in 0..BLOCKS_LIMIT {
        rig.events
            .push(Frame::new(rig.frame), trigger)
            .expect("room");
        rig.block();
        let reports = rig.pump();
        rig.block();
        if reports
            .iter()
            .any(|r| matches!(r, PluginInvalidation::RateRestored))
        {
            break;
        }
    }
    let calls = rig.fake.calls();
    let at_c = calls
        .iter()
        .position(|c| matches!(c, Call::Activate(rate, _) if *rate == C))
        .expect("the plugin was activated at the accepted rate");
    assert!(at_c > rolled_back);
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
    let note = midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100));
    for _ in 0..3 {
        rig.events.push(Frame::new(rig.frame), note).expect("room");
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
