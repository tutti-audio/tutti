//! A render prepared `Realtime` runs inside `RtScope`; one prepared `Offline`
//! does not, serial or on a pool.
//!
//! A node that allocates in `process` aborts the process under the area
//! binary's `RtAllocCheck` when the executor marked the thread, so each case
//! runs as a child of this binary (the pattern of `tutti-fixtures`'
//! `rt_alloc_check`) and the test reads the child's exit status.

use std::process::Command;

use tutti_buffers::ChannelLayout;
use tutti_graph::testing::{self as common, prepare};
use tutti_graph::{
    Cx, Editor, Executor, Io, Node, NodeKey, OutputPort, PortSource, Prepare, RenderMode, Shape,
    Status, TransportPosition, Unforkable,
};

const BLOCK: usize = 32;
/// Independent sources, so a pool's helpers have nodes to take.
const SOURCES: usize = 8;
const CHILD: &str = "TUTTI_RENDER_MODE_SCOPE_CHILD";
const CASE: &str =
    "render_mode_scope::an_allocating_node_aborts_a_realtime_render_and_renders_offline";

/// A source that allocates and frees in every `process`.
struct Allocating;

impl Node for Allocating {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        drop(std::hint::black_box(vec![1_u8; 64]));
        io.output(0).fill(0.25);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Eight allocating sources, one output channel each, prepared in `mode`, on a
/// pool of `participants` (`1`: serial), rendered for a few blocks.
fn render(mode: RenderMode, participants: usize) {
    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(BLOCK).with_render_mode(mode));
    if participants > 1 {
        exec.set_pool(Some(common::test_pool(participants)));
    }
    for k in 0..SOURCES as u64 {
        ed.insert(NodeKey(k), "alloc", Unforkable(Allocating));
    }
    ed.spec_mut()
        .topology
        .set_outputs((0..SOURCES as u64).map(|k| {
            PortSource::Node(OutputPort {
                node: NodeKey(k),
                port: 0,
            })
        }));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut out = vec![[0.0f32; BLOCK]; SOURCES];
    for _ in 0..8 {
        let mut refs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        exec.process(BLOCK, &TransportPosition::default(), &[], &mut refs);
    }
    assert!(
        out.iter().all(|c| c.iter().all(|&s| s == 0.25)),
        "every source rendered"
    );
}

/// **The mode of the prepare decides the scope.** An allocating node aborts
/// a `Realtime` render, and renders under an `Offline` one, serial and on a
/// pool of two participants.
#[test]
fn an_allocating_node_aborts_a_realtime_render_and_renders_offline() {
    if let Ok(case) = std::env::var(CHILD) {
        match case.as_str() {
            "realtime" => render(RenderMode::Realtime, 1),
            "realtime_pool" => render(RenderMode::Realtime, 2),
            "offline" => render(RenderMode::Offline, 1),
            _ => render(RenderMode::Offline, 2),
        }
        return;
    }
    let run = |case: &str| {
        Command::new(std::env::current_exe().expect("the test binary's path"))
            .args(["--exact", CASE, "--nocapture"])
            .env(CHILD, case)
            .output()
            .expect("the child runs")
    };
    for case in ["realtime", "realtime_pool"] {
        let out = run(case);
        assert!(
            !out.status.success(),
            "an allocation in a realtime render aborts ({case})"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("inside an audio block"),
            "the report names the cause ({case}): {err}"
        );
    }
    for case in ["offline", "offline_pool"] {
        let out = run(case);
        assert!(
            out.status.success(),
            "an offline render may allocate ({case}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
