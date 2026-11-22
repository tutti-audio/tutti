//! A bounce at a sample rate a node does not run at is refused, naming the
//! node, its rates and the rate asked for.

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, ForkByClone, ForkError, Io, Node, Prepare, SampleRates, Shape, Status};
use tutti_session::{Bounce, BounceError, DetachedSession, MainOutput, Range, Session};
use tutti_units::{SampleRate, Seconds};

use crate::bounced::bounced;

/// A source of silence that runs at `rates` only.
#[derive(Clone)]
struct Rated(SampleRates);

impl Node for Rated {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO).with_rates(self.0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        io.output(1).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn bounce_at(session: &mut DetachedSession, rate: SampleRate) -> Result<usize, BounceError> {
    let bounce = Bounce::new()
        .main_output_buffers()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.25)))
        .sample_rate(rate);
    Ok(bounced(session, bounce)?.into_main_output().into_planes()[0].len())
}

/// **A fork refuses a rate outside a node's declared rates and renders at one
/// inside them.** At 96 kHz the bounce is `RateUnsupported` with the node's
/// key and exactly its range; at 48 kHz, the range's top, it renders.
#[test]
fn a_bounce_at_a_rate_a_node_does_not_run_at_is_refused() {
    let mut session = Session::builder()
        .inputs(ChannelLayout::EMPTY)
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    let rates = SampleRates::of(SampleRate::SR_44K1, SampleRate::SR_48K);
    let node = session.graph().add(ForkByClone(Rated(rates)));
    session.graph().connect(node, MainOutput).unwrap().unfaded();

    let refused = bounce_at(&mut session, SampleRate::SR_96K).unwrap_err();
    assert!(
        matches!(
            refused,
            BounceError::Fork(ForkError::RateUnsupported { key, supported, rate })
                if key == node.erase() && supported == rates && rate == SampleRate::SR_96K
        ),
        "{refused:?}"
    );
    assert_eq!(
        bounce_at(&mut session, SampleRate::SR_48K).unwrap(),
        (0.25 * 48_000.0) as usize
    );
}
