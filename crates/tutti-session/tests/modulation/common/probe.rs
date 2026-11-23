//! Nodes for the modulation suite (design 015 §5.1: the session's tests have
//! their own probe nodes).
//!
//! - [`ProbeUnit`]: a host's `ParamNode` with one param (its knob a `Param`
//!   cell, in its `ParamSet`) that writes the value its param port reads to
//!   its one output, so a render shows what a lane delivers.
//! - [`Pulse`]: a stereo level on a span of executor frames, a follower's
//!   input.
//!
//! `ProbeUnit` is a [`SessionNode`] whose controls are its `ParamSet`, so
//! `Graph::add` addresses its param.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use atomic_float::AtomicF64;
use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_session::SessionNode;
use tutti_units::{Drive, Hz, UnitParam};

// A probe's kind is chosen when it is built; its type declares the ones the
// tests write by key (a probe that does not have one refuses the write).
// Its params are chosen when it is built and its knobs are raw atomics, so a
// declaration has no cell to check a unit against: its typed side is written
// by hand.
impl tutti_session::HasParam<{ UnitParam::Cutoff as u16 }> for ProbeUnit {
    type Unit = Hz;
}
impl tutti_session::HasParam<{ UnitParam::Drive as u16 }> for ProbeUnit {
    type Unit = Drive;
}

/// One param (`Cutoff` unless told) whose knob is a cell, and an output that
/// carries the value the param port reads: the port's frames under
/// modulation, the knob without it. `extra` declares a second param after
/// it, which the probe does not read. `Clone` shares the cells.
#[derive(Clone)]
pub struct ProbeUnit {
    param: UnitParam,
    extra: Option<UnitParam>,
    knob: Arc<AtomicF64>,
    other: Arc<AtomicF64>,
}

impl ProbeUnit {
    /// A cutoff probe at `knob`.
    pub fn new(knob: f64) -> Self {
        Self::of(UnitParam::Cutoff, knob)
    }

    /// A probe of `param` at `knob`.
    pub fn of(param: UnitParam, knob: f64) -> Self {
        Self {
            param,
            extra: None,
            knob: Arc::new(AtomicF64::new(knob)),
            other: Arc::new(AtomicF64::new(0.0)),
        }
    }

    /// With a second param, `extra`, after the first: another param list.
    pub fn with_extra(mut self, extra: UnitParam) -> Self {
        self.extra = Some(extra);
        self
    }

    /// The knob cell, shared with every clone.
    pub fn knob(&self) -> Arc<AtomicF64> {
        Arc::clone(&self.knob)
    }
}

impl Node for ProbeUnit {
    fn shape(&self) -> Shape {
        let shape = Shape::source_unassigned(ChannelLayout::MONO);
        match self.extra {
            None => shape.params(&[self.param]),
            Some(extra) => shape.params(&[self.param, extra]),
        }
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0).frames() {
            Some(v) => io.output(0).copy_from_slice(v),
            None => {
                let k = self.knob.load(Ordering::Acquire);
                io.output(0).fill(k as f32);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}

    fn param_base(&self, port: usize) -> Option<f64> {
        match port {
            0 => Some(self.knob.load(Ordering::Acquire)),
            1 if self.extra.is_some() => Some(self.other.load(Ordering::Acquire)),
            _ => None,
        }
    }
}

impl ParamNode for ProbeUnit {
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder().param(
            tutti_graph::DeclaredUnit::of::<Hz>(),
            self.param,
            Arc::clone(&self.knob),
        );
        match self.extra {
            None => set.build(),
            Some(extra) => set
                .param(
                    tutti_graph::DeclaredUnit::of::<Hz>(),
                    extra,
                    Arc::clone(&self.other),
                )
                .build(),
        }
    }

    fn fork_fresh(&self) -> Self {
        Self {
            param: self.param,
            extra: self.extra,
            knob: Arc::new(AtomicF64::new(self.knob.load(Ordering::Acquire))),
            other: Arc::new(AtomicF64::new(self.other.load(Ordering::Acquire))),
        }
    }
}

impl IntoNode for ProbeUnit {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl SessionNode for ProbeUnit {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }

    fn reclaimer(_: &ParamSet) -> tutti_session::Reclaims {
        tutti_session::Reclaims::Nothing
    }
}

/// `level` on executor frames `[from, to)`, else 0, on both channels.
#[derive(Clone)]
pub struct Pulse {
    pub from: u64,
    pub to: u64,
    pub level: f32,
}

impl Node for Pulse {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let base = cx.env.frame.get();
        for c in 0..2 {
            for (i, x) in io.output(c).iter_mut().enumerate() {
                let f = base + i as u64;
                *x = if (self.from..self.to).contains(&f) {
                    self.level
                } else {
                    0.0
                };
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}
