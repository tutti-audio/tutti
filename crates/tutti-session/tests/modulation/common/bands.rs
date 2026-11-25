//! A node with two params of one kind, for the indexed-address suite.
//!
//! [`Bands`] addresses a `Threshold` at index 0 and another at index 1, each a
//! cell of its own, and writes what port 0 reads to output 0 and what port
//! 1 reads to output 1 (the port's frames under modulation, the knob
//! without it). It is a `ParamNode`, inserted as `Params(Bands::new(..))`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use atomic_float::AtomicF64;
use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Io, Node, ParamInput, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_units::Db;
use tutti_units::{UnitAddr, UnitParam};

/// `Threshold` of band 0 and of band 1.
pub(crate) const BAND0: UnitAddr = UnitAddr::new(UnitParam::Threshold, 0);
pub(crate) const BAND1: UnitAddr = UnitAddr::new(UnitParam::Threshold, 1);

/// Two thresholds, as above. `Clone` shares the cells.
#[derive(Clone)]
pub(crate) struct Bands {
    knobs: [Arc<AtomicF64>; 2],
}

impl Bands {
    /// Band 0 at `t0`, band 1 at `t1`.
    pub(crate) fn new(t0: f64, t1: f64) -> Self {
        Self {
            knobs: [Arc::new(AtomicF64::new(t0)), Arc::new(AtomicF64::new(t1))],
        }
    }
}

impl Node for Bands {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO).param_addrs(&[BAND0, BAND1])
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (k, knob) in self.knobs.iter().enumerate() {
            match io.param(k) {
                ParamInput::Frames { values: v, .. } => io.output(k).copy_from_slice(v),
                ParamInput::Base => {
                    let base = knob.load(Ordering::Acquire);
                    io.output(k).fill(base as f32);
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}

    fn param_base(&self, port: usize) -> Option<f64> {
        self.knobs.get(port).map(|k| k.load(Ordering::Acquire))
    }
}

impl ParamNode for Bands {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(
                tutti_graph::DeclaredUnit::of::<Db>(),
                BAND0,
                Arc::clone(&self.knobs[0]),
            )
            .param(
                tutti_graph::DeclaredUnit::of::<Db>(),
                BAND1,
                Arc::clone(&self.knobs[1]),
            )
            .build()
    }

    fn fork_fresh(&self) -> Self {
        Self::new(
            self.knobs[0].load(Ordering::Acquire),
            self.knobs[1].load(Ordering::Acquire),
        )
    }
}

// Its knobs are raw atomics, not `Param<Db>` cells a declaration could check
// the unit against, so its typed side is written by hand.
impl tutti_session::HasParam<{ UnitParam::Threshold as u16 }> for Bands {
    type Unit = Db;
}
impl tutti_session::HasParams<{ UnitParam::Threshold as u16 }> for Bands {}
