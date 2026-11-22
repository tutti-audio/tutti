//! A node that parks the third block it runs in until the control thread lets it
//! go: a store landing while a block runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};

/// Outputs `1.0`, and at its third call waits inside `process` for
/// [`Release::release`]. The handshake is two atomics and a spin, so `process`
/// takes no lock, does not allocate and does not sleep.
pub(crate) struct Rendezvous {
    calls: usize,
    inside: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

/// The control thread's end of a [`Rendezvous`].
#[derive(Clone)]
pub(crate) struct Release {
    inside: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl Rendezvous {
    pub(crate) fn new() -> (Self, Release) {
        let inside = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        (
            Self {
                calls: 0,
                inside: Arc::clone(&inside),
                release: Arc::clone(&release),
            },
            Release { inside, release },
        )
    }
}

impl Release {
    /// Spins until the node is inside its third `process`, then runs `store`
    /// and lets the block go.
    pub(crate) fn store_while_inside(&self, store: impl FnOnce()) {
        while !self.inside.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        store();
        self.release.store(true, Ordering::Release);
    }
}

impl Node for Rendezvous {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls += 1;
        if self.calls == 3 {
            self.inside.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
        }
        io.output(0).fill(1.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}
