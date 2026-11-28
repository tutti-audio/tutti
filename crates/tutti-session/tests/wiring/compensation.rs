//! The compensation figures a session publishes (the pre-roll table and the
//! total latency): what a commit sends is what is published, a clean update
//! publishes nothing, a re-prepare republishes once it resumes, and the
//! compiler compensates without splicing anything into the wiring.
//!
//! The
//! total and the table of a latent branch are also pinned, to the sample, by
//! `graph.rs`'s `a_latent_branch_is_compensated_on_the_other_to_the_sample`,
//! and a restart's republication by `device.rs`'s
//! `a_rerate_keeps_the_published_compensation_in_step_with_the_plan`.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_nodes::{LimiterConfig, LimiterControls, LimiterNode};
use tutti_session::{DetachedSession, InputPort, MainOutput, OutputPort, Session};
use tutti_units::{Db, SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_44K1;

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("builds")
}

/// A limiter's lookahead is the latency; the one the engine ships.
fn limiter() -> LimiterNode<f32> {
    LimiterNode::<f32>::from(
        LimiterConfig::new(ChannelTopology::MONO).controls(LimiterControls {
            threshold: Db::new(-1.0),
            ceiling: Db::new(-0.3),
            ..LimiterControls::DEFAULT
        }),
    )
}

/// ch0 through a limiter, ch1 dry: ch1's source must pre-roll to match.
/// Returns the limiter's node latency.
fn skewed(s: &mut DetachedSession) -> (tutti_graph::NodeKey, Samples) {
    let a = s.graph().add(ForkByClone(Const::mono(1.0)));
    let eff = s.graph().add(limiter());
    let b = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .set_sources(InputPort::new(eff, 0), [OutputPort::new(a, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(eff, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(b, 0)])
        .unwrap();
    let lat = s.graph_ref().node_latency(eff).unwrap();
    // Every test built on this graph compares against `lat`; if the limiter
    // ever reported no latency they would all agree on zero and pass while
    // testing nothing.
    assert!(lat.get() > 0, "LimiterNode must report its lookahead");
    (eff.erase(), lat)
}

/// **A graph where nothing reports latency has no figure to display**: the
/// total is zero and the table has a zero per output.
#[test]
fn a_graph_with_no_latency_reports_none() {
    let mut s = detached();
    let a = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(a, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(a, 0)])
        .unwrap();
    let _ = s.update();

    assert_eq!(s.latency(), Samples::new(0));
    assert!(
        s.compensation()
            .to_vec()
            .iter()
            .all(|c| *c == Samples::new(0)),
        "no channel pre-rolls"
    );
}

/// **The published figures are the plan's**: what a commit publishes is
/// exactly the `Plan::compensation` / `total_latency` it sent the executor,
/// so a pre-roll a source outside the graph reads cannot drift off the delay
/// the graph applies. The channel that carries the limiter pre-rolls by zero
/// (the dry one by its lookahead), and the total is the lookahead: the figure
/// a DAW displays is exactly the one the table does not contain. Nothing is
/// spliced in to get there: both outputs still read the nodes they were
/// wired to.
#[test]
fn the_published_figures_are_the_plans_and_nothing_is_spliced_in() {
    let mut s = detached();
    let (_, lat) = skewed(&mut s);
    let authored = [
        s.graph_ref().sources(MainOutput::channel(0)).unwrap(),
        s.graph_ref().sources(MainOutput::channel(1)).unwrap(),
    ];
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits(), "committed");

    assert_eq!(s.latency(), lat, "total latency is the limiter's lookahead");
    assert_eq!(
        s.compensation().to_vec().first().copied(),
        Some(Samples::new(0)),
        "the channel that defines the latency pre-rolls by zero"
    );
    assert_eq!(
        s.compensation().to_vec().get(1).copied(),
        Some(lat),
        "the dry channel pre-rolls by the lookahead"
    );
    assert_eq!(
        [
            s.graph_ref().sources(MainOutput::channel(0)).unwrap(),
            s.graph_ref().sources(MainOutput::channel(1)).unwrap()
        ],
        authored,
        "nothing is spliced into the wiring"
    );
}

/// **A re-prepare to a new rate republishes the figures once it resumes**,
/// with nothing else marking the graph edited: the limiter's lookahead is a
/// time, so its latency in samples moves with the rate, and the resumed plan
/// compensates by the new figure: the update whose collect resumes the plan
/// publishes its figures.
#[test]
fn a_re_prepare_republishes_the_figures_once_it_resumes() {
    let mut s = detached();
    let a = s.graph().add(ForkByClone(Const::mono(1.0)));
    let eff = s.graph().add(limiter());
    s.graph()
        .set_sources(InputPort::new(eff, 0), [OutputPort::new(a, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(eff, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(a, 0)])
        .unwrap();
    let at_44k = s.graph_ref().node_latency(eff).unwrap();
    let mut side = s.graph().take_audio_side();
    let _ = s.update();
    assert_eq!(s.latency(), at_44k);
    let mut out = [0.0f32; 2];
    side.tick(InterleavedMut::one_frame(&mut out).unwrap());

    s.set_sample_rate(SampleRate::SR_48K).unwrap();
    for _ in 0..3 {
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
        let _ = s.update();
    }
    let at_48k = s.graph_ref().node_latency(eff).unwrap();
    assert_ne!(at_48k, at_44k, "the lookahead moved in samples");
    assert_eq!(s.latency(), at_48k);
    assert_eq!(
        s.compensation().to_vec().get(1),
        Some(&at_48k),
        "the dry channel pre-rolls by the new figure"
    );
}
