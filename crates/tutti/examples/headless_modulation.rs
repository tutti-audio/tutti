//! Headless modulation: an LFO and an envelope
//! follower on a filter's native params, declared on an offline session
//! ([`Session::mods`]) and applied by its update.
//!
//! A saw runs through a low-pass SVF. A 2 Hz sine LFO sweeps the cutoff;
//! a follower of a kick (a decaying burst on every half second) raises the
//! resonance on each hit. Then a depth edit republishes the LFO's lane with
//! no plan sent. Prints the output's level per 50 ms window.
//!
//! ```text
//! cargo run -p tutti --example headless_modulation
//! ```
//!
//! The umbrella's, so that it imports as an application does, from `tutti`'s
//! root; what the root does not carry (the node contract, the SVF
//! filter) is named through `tutti::graph` and `tutti::nodes`; the testing
//! oscillator is `tutti-nodes`'s, a dev-dependency of this crate.

use tutti::graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti::modulation::{Follower, Lfo};
use tutti::nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti::{
    Bipolar, ChannelLayout, DepthTarget, Error, Hz, InputPort, InterleavedMut, MainOutput,
    MaxBlock, OutputPort, Q, RouteSpec, SampleRate, Samples, Session, SessionEvent,
};
use tutti_nodes::testing::Osc;

const RATE: f64 = 48_000.0;

/// What `Session::update` found: faults and dropouts (a refused commit, a
/// plugin that died, a lost block). A host logs them; here they are printed.
fn report(events: Vec<SessionEvent>) {
    for event in events {
        eprintln!("session event: {event:?}");
    }
}
const BLOCK: usize = 512;

/// A kick on the executor's own clock: a burst decaying over 50 ms at the
/// top of every half second.
#[derive(Clone)]
struct Kick;

impl Node for Kick {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let base = cx.env.frame.get();
        for (i, x) in io.output(0).iter_mut().enumerate() {
            let t = (base + i as u64) % 24_000;
            *x = (1.0 - t as f32 / 2_400.0).max(0.0);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// `windows` 50 ms windows, printed as their RMS. The session updates
/// between blocks, as a host's frame loop would.
fn play(s: &mut Session, windows: usize) {
    let window = 2_400;
    let mut out = vec![0.0f32; BLOCK];
    let (mut sum, mut n) = (0.0f64, 0usize);
    let mut line = String::new();
    while n < windows * window {
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
        report(s.update());
        for &x in &out {
            sum += f64::from(x) * f64::from(x);
            n += 1;
            if n % window == 0 {
                line.push_str(&format!("{:.3} ", (sum / window as f64).sqrt()));
                sum = 0.0;
            }
        }
    }
    println!("{line}");
}

fn main() -> Result<(), Error> {
    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .outputs(ChannelLayout::MONO)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("builds");
    let saw = s.graph().add(ForkByClone(Osc::saw(Hz::new(110.0))));
    let filter = s.graph().add(SvfFilterNode::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let kick = s.graph().add(ForkByClone(Kick));
    s.graph()
        .set_sources(InputPort::new(filter, 0), [OutputPort::new(saw, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(filter, 0)])
        .unwrap();

    let cutoff = filter.cutoff();
    let resonance = filter.q();
    let mut m = s.mods_mut();
    m.range(cutoff, Hz::new(100.0)..Hz::new(4_000.0))?;
    m.range(resonance, Q::new(0.5)..Q::new(8.0))?;
    let wobble = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    m.route(RouteSpec::new(wobble, cutoff, Bipolar::of(0.2)))?;
    let hits = m.follower(Follower::default(), kick);
    m.route(RouteSpec::new(hits, resonance, Bipolar::of(0.5)))?;

    // The first update's one plan carries the nodes and their lanes (a target
    // resolves as soon as its node is staged).
    report(s.update());
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "the declarations apply"
    );
    println!("LFO depth 0.2, kick on the resonance:");
    play(&mut s, 20);

    // A depth edit: the LFO's lane reads its depth from a cell, so the wave
    // goes on in phase. It edits no wiring, so the graph holds nothing for a
    // commit and no plan is sent.
    s.set(DepthTarget::of(wobble, cutoff).to(Bipolar::of(0.45)))
        .expect("the route is declared");
    assert!(
        !s.graph_ref().has_uncommitted_edits(),
        "a depth edit sends no plan"
    );
    report(s.update());
    println!("LFO depth 0.45:");
    play(&mut s, 20);
    Ok(())
}
