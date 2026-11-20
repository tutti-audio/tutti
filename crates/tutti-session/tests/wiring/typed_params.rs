//! Typed params on keys (doc 030, T2, T3, N2, N3): `lp.cutoff()` is a
//! reference whose value is in the unit the node declares, `Session::set`
//! takes it with a value of that unit and says why it refused, and a host's
//! own `ParamNode` is inserted as `Params(node)` with no hand-written
//! `SessionNode`.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_graph::{
    Cx, FadeLaw, Io, Node, ParamNode, ParamSet, Params, Prepare, Shape, Status, Unforkable,
};
use tutti_nodes::{
    CompressorConfig, CompressorControls, CompressorNode, KeySource, SvfFilterConfig,
    SvfFilterNode, SvfType,
};
use tutti_rt::Param;
use tutti_session::{DetachedSession, Session, ValueErrorKind};
use tutti_units::{Db, Hz, Q, SampleRate, Seconds, UnitAddr, UnitParam};

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// **A key's param method names the param it is called for, at the address
/// the node declares it at**, whatever the node: the kind is the method's,
/// the index 0, and the node is the key's. (An index other than 0 is a
/// node's `_at` method, held in `modulation/indexed_params.rs`.)
#[test]
fn a_keys_param_methods_name_their_kind_and_index() {
    let mut s = detached();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let cutoff = lp.cutoff();
    assert_eq!(cutoff.node(), lp.erase());
    assert_eq!(cutoff.addr(), UnitAddr::first(UnitParam::Cutoff));
    assert_eq!(lp.q().addr(), UnitAddr::first(UnitParam::Q));
    assert_eq!(lp.gain_db().addr(), UnitAddr::first(UnitParam::GainDb));
}

/// **A write through a key's param reaches the node's cell, in its unit**:
/// the filter's own control reads what `set` wrote.
#[test]
fn a_write_reaches_the_cell_the_key_names() {
    let mut s = detached();
    let (lp, controls) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    s.set(lp.cutoff().to(Hz::new(2_500.0))).unwrap();
    s.set(lp.q().to(Q::new(1.5))).unwrap();
    assert_eq!(controls.get(UnitParam::Cutoff), Some(2_500.0));
    assert_eq!(controls.get(UnitParam::Q), Some(1.5));
    assert_eq!(controls.get(UnitParam::GainDb), Some(0.0), "the rest stays");

    // A `Seconds` param is an `f64` cell: the write is exact for a value an
    // `f32` holds.
    let (comp, controls) = s.graph().add(CompressorNode::<f32>::from(
        CompressorConfig::new(BusTopology::MONO)
            .key(KeySource::Sidechain)
            .controls(CompressorControls {
                threshold: Db::new(-20.0),
                ratio: tutti_units::CompressionRatio::new(4.0),
                attack: Seconds::new(0.01),
                release: Seconds::new(0.1),
                ..CompressorControls::DEFAULT
            }),
    ));
    s.set(comp.attack().to(Seconds::new(0.25))).unwrap();
    assert_eq!(controls.get(UnitParam::Attack), Some(0.25));
}

/// **A write that reaches nothing is refused, saying which**: a node that is
/// not in the graph, and a node with no such param addressed (the filter's
/// key after `replace` swapped in an `Unforkable` one, which addresses no
/// params). Nothing is written.
#[test]
fn a_write_to_a_missing_node_or_param_is_no_such_target() {
    let mut s = detached();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let (bare, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    // `replace` addresses what the incoming node declares: an `Unforkable`
    // declares nothing, so the key's cutoff now reaches no param.
    s.graph()
        .replace(
            bare,
            Unforkable(SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                    .freq(Hz::new(1_000.0))
                    .q(Q::new(0.7)),
            )),
            Seconds::new(0.0),
            FadeLaw::EqualPower,
        )
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));
    let refused = s.set(bare.cutoff().to(Hz::new(5.0))).unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::NoSuchTarget);
    assert!(
        refused
            .target
            .expect("it names the target")
            .is(bare.cutoff())
    );

    let cutoff = lp.cutoff();
    s.graph().remove(lp).unwrap();
    let refused = s.set(cutoff.to(Hz::new(5.0))).unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::NoSuchTarget);
    assert_eq!(refused.at, 0);
    assert_eq!(
        refused.target.expect("it names the key").node(),
        Some(lp.erase())
    );
}

/// A host's node with one param, a `Hz` cutoff.
#[derive(Clone)]
struct Mine {
    cutoff: Param<Hz>,
}

impl Node for Mine {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        // An output sample is an `f32`.
        io.output(0).fill(self.cutoff.load().get() as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl ParamNode for Mine {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.cutoff.detach();
        fork
    }
}

tutti_graph::declare_params!([] Mine, n { Cutoff => Hz = &n.cutoff });

/// **A host's `ParamNode` is inserted as `Params(node)` and nothing more**:
/// no `IntoNode` and no `SessionNode` of its own. Its params are addressed
/// (`set_param` reaches it), its key has the typed method its declaration
/// gives, and a fork of it starts from the value set.
#[test]
fn a_params_wrapped_node_is_addressed_and_forks_from_its_values() {
    let mut s = detached();
    let (mine, controls) = s.graph().add(Params(Mine {
        cutoff: Param::new(Hz::new(100.0)),
    }));
    s.graph()
        .set_sources_unfaded(tutti_session::Root::output(0), [mine.out(0)])
        .unwrap();
    let _ = s.update();

    s.set(mine.cutoff().to(Hz::new(440.0))).unwrap();
    assert_eq!(controls.get(UnitParam::Cutoff), Some(440.0));
    let mut live = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut live).unwrap());
    assert_eq!(live, [440.0], "the live node reads it");

    let mut fork = s
        .graph_ref()
        .testing()
        .fork_live()
        .expect("a Params node forks");
    s.set(mine.cutoff().to(Hz::new(880.0))).unwrap();
    let mut forked = [0.0f32];
    fork.tick(InterleavedMut::one_frame(&mut forked).unwrap());
    assert_eq!(forked, [440.0], "the fork has the value set at the fork");
}
